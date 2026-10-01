//! The response vocabulary Codex's `PreToolUse` hook actually accepts.
//!
//! # Why this module exists
//!
//! Boss's guard scripts are written in Claude Code's hook dialect, which Codex
//! rejects, so [`super::guard_trace`]'s shim translates their stdout rather
//! than re-emitting it. An untranslated `{"decision":"approve"}` produces one
//! `PreToolUse hook returned unsupported decision:approve` per armed guard, on
//! every tool call.
//!
//! The rejection is **fail-open**. Codex logs the hook as failed and runs the
//! tool call anyway, so a rejected response is indistinguishable from an
//! approval — which is why a `block` that Codex rejects is a silently inert
//! guard, not merely noise.
//!
//! # Scope: this describes codex-cli, and nothing else
//!
//! The vocabulary below is `codex-cli`'s `PreToolUse` dialect specifically, and
//! this module is deliberately private to [`super`]. It is not a shared
//! cross-driver contract, and a new driver must **not** reuse it: the sibling
//! Grok driver's `driver::grok::hooks::translate_decision` speaks a genuinely
//! different dialect, and a third agent's is unknown until someone measures it.
//! What is reusable is the *shape* — measure the target agent's accepted
//! responses, encode them as executable data next to that driver, and have the
//! driver's tests ask "would this agent accept what we just emitted?". Tests
//! must assert that the target agent accepts what a guard emits, not that a
//! guard fires; only the former catches a dialect mismatch.
//!
//! # The contract, as measured
//!
//! Every row below was measured against `codex-cli 0.145.0` by arming a single
//! `.*` `PreToolUse` guard that printed the payload and running one real
//! `codex exec` turn. Evidence:
//! `tools/boss/docs/investigations/codex-pretooluse-decision-vocabulary-2026-07-30.md`.
//!
//! | guard stdout                                                        | Codex     | tool call |
//! | ------------------------------------------------------------------- | --------- | --------- |
//! | *(nothing)*                                                          | accepted  | proceeds  |
//! | `{}`                                                                 | accepted  | proceeds  |
//! | `{"continue": true}`                                                 | accepted  | proceeds  |
//! | `{"decision":"block","reason":"…"}`                                  | accepted  | **blocked** |
//! | `{"hookSpecificOutput":{…,"permissionDecision":"deny","permissionDecisionReason":"…"}}` | accepted | **blocked** |
//! | `{"decision":"approve"}`                                             | rejected  | proceeds  |
//! | `{"decision":"allow"}`                                               | rejected  | proceeds  |
//! | `{"decision":"deny","reason":"…"}`                                   | rejected  | proceeds  |
//! | `{"decision":"block"}` *(no reason)*                                 | rejected  | proceeds  |
//! | `{"decision":"block","reason":""}`                                   | rejected  | proceeds  |
//! | `{"hookSpecificOutput":{…,"permissionDecision":"allow"}}`            | rejected  | proceeds  |
//! | `{"suppressOutput": true}`                                           | rejected  | proceeds  |
//!
//! One row is not from that matrix: `{"continue": false}` is refused, per the
//! binary's own validation string `PreToolUse hook returned unsupported
//! continue:false`. Only `continue:true` was exercised live, and it is accepted
//! — so `continue` is rejected on its false value alone.
//!
//! Two consequences drive the shim's translation:
//!
//! 1. **There is no affirmative allow token.** `approve`, `allow` and
//!    `permissionDecision:allow` are all refused, so the only way to say "let
//!    this through" is to say nothing.
//! 2. **A block's reason is load-bearing.** `{"decision":"block"}` and an empty
//!    reason are both rejected, and rejection means the call runs. A guard that
//!    blocks tersely would be silently disarmed, so the shim substitutes a
//!    reason rather than emitting one Codex will throw away.

/// Substituted when a refusal arrives with no reason attached.
///
/// Mirrors `BLOCK_WITHOUT_REASON` in the shim; both exist because a reasonless
/// block is rejected, and a rejected block runs the call. The two copies are
/// held together by `guard_trace`'s
/// `a_guard_that_blocks_without_saying_why_still_blocks`, which runs the real
/// shim and asserts the substituted text equals this constant.
pub(super) const BLOCK_WITHOUT_REASON: &str =
    "Blocked by a Boss guard, which did not record a reason. Treat this as a refusal and do not retry the call";

/// Render a refusal in the one shape Codex accepts.
///
/// The single place Rust-side code builds a `PreToolUse` refusal, so "a block
/// carries a non-empty reason" is guaranteed by construction rather than by
/// every caller remembering. Callers that already have a reason keep it.
pub(super) fn block_response(reason: &str) -> String {
    let reason = if reason.trim().is_empty() {
        BLOCK_WITHOUT_REASON
    } else {
        reason
    };
    serde_json::json!({"decision": "block", "reason": reason}).to_string()
}

/// What Codex does with one guard response on the `PreToolUse` path.
///
/// The test oracle for the contract documented above: it exists so tests can
/// ask "would Codex accept this?" instead of restating a literal. Production
/// code emits through [`block_response`] and the shim's `emit_decision`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Accepted, and the tool call proceeds.
    Allow,
    /// Accepted, and the tool call is refused with this reason.
    Block(String),
    /// Refused as malformed. Codex reports the hook as failed and **runs the
    /// tool call anyway**, so every rejection is a fail-open — which is what
    /// makes this variant a test failure wherever Boss produces it.
    Rejected(String),
}

/// Top-level keys Codex refuses outright, whatever their value.
///
/// `continue` is not one of them — it is refused on its `false` value alone,
/// which [`verdict`] handles separately. `updatedInput` is absent too: the
/// binary's string is `PreToolUse hook returned updatedInput without
/// permissionDecision:allow`, which says it is refused *without* an allow that
/// is itself refused — not that the key is unsupported on its own. Nobody has
/// measured a bare `updatedInput`, so this table does not claim to know.
#[cfg(test)]
const UNSUPPORTED_KEYS: &[&str] = &["stopReason", "suppressOutput"];

/// Decide what Codex would do with `stdout` from a `PreToolUse` guard.
///
/// This mirrors the measured contract above rather than Codex's internals; it
/// exists so tests can assert on the agent's acceptance rather than on a
/// literal string Boss happens to emit today.
#[cfg(test)]
pub(super) fn verdict(stdout: &str) -> Verdict {
    let text = stdout.trim();
    // Silence is the allow path, and the only allow path.
    if text.is_empty() {
        return Verdict::Allow;
    }
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return Verdict::Rejected(format!("output is not JSON: {text:?}"));
    };
    let Some(object) = parsed.as_object() else {
        return Verdict::Rejected("output is not a JSON object".to_owned());
    };

    for key in UNSUPPORTED_KEYS {
        if object.contains_key(*key) {
            return Verdict::Rejected(format!("unsupported {key}"));
        }
    }

    // `continue` is refused on its false value only; `continue:true` measured
    // as accepted.
    if object.get("continue") == Some(&serde_json::Value::Bool(false)) {
        return Verdict::Rejected("unsupported continue:false".to_owned());
    }

    // `hookSpecificOutput` dialect, checked first: it carries its decision and
    // its reason under their own keys.
    if let Some(hook_output) = object.get("hookSpecificOutput").and_then(serde_json::Value::as_object) {
        match hook_output
            .get("permissionDecision")
            .and_then(serde_json::Value::as_str)
        {
            Some("deny") => {
                let reason = hook_output
                    .get("permissionDecisionReason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                return if reason.trim().is_empty() {
                    Verdict::Rejected("permissionDecision:deny without a non-empty permissionDecisionReason".to_owned())
                } else {
                    Verdict::Block(reason.to_owned())
                };
            }
            Some(other) => return Verdict::Rejected(format!("unsupported permissionDecision:{other}")),
            None => {}
        }
    }

    match object.get("decision").and_then(serde_json::Value::as_str) {
        // `block` is the one accepted `decision` value -- note `deny` is not.
        Some("block") => {
            let reason = object
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if reason.trim().is_empty() {
                Verdict::Rejected("decision:block without a non-empty reason".to_owned())
            } else {
                Verdict::Block(reason.to_owned())
            }
        }
        Some(other) => Verdict::Rejected(format!("unsupported decision:{other}")),
        None if object.contains_key("reason") => Verdict::Rejected("reason without decision".to_owned()),
        None => Verdict::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rejection messages the shipping Codex binary carries verbatim. Used by
    /// [`the_model_matches_the_shipping_binarys_own_rejection_list`] to catch
    /// the model drifting away from the CLI it describes.
    const BINARY_REJECTION_STRINGS: &[&str] = &[
        "PreToolUse hook returned unsupported decision:approve",
        "PreToolUse hook returned unsupported permissionDecision:allow",
        "PreToolUse hook returned unsupported permissionDecision:ask",
        "PreToolUse hook returned unsupported stopReason",
        "PreToolUse hook returned unsupported suppressOutput",
        "PreToolUse hook returned unsupported continue:false",
        "PreToolUse hook returned reason without decision",
        "PreToolUse hook returned permissionDecision:deny without a non-empty permissionDecisionReason",
    ];

    #[test]
    fn silence_is_the_allow_path() {
        assert_eq!(verdict(""), Verdict::Allow);
        assert_eq!(verdict("\n  \n"), Verdict::Allow);
        assert_eq!(verdict("{}"), Verdict::Allow);
    }

    #[test]
    fn claudes_allow_vocabulary_is_rejected() {
        // The bug this module exists for: all three of Claude's ways of saying
        // "proceed" are refused, and refusal runs the call anyway.
        for emitted in [
            r#"{"decision":"approve"}"#,
            r#"{"decision":"allow"}"#,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}"#,
        ] {
            assert!(
                matches!(verdict(emitted), Verdict::Rejected(_)),
                "{emitted} must be rejected"
            );
        }
    }

    #[test]
    fn a_block_is_accepted_only_with_a_non_empty_reason() {
        assert_eq!(
            verdict(r#"{"decision":"block","reason":"use cube"}"#),
            Verdict::Block("use cube".to_owned())
        );
        // Both of these are rejected, which means the tool call *runs*. A
        // terse guard is a disarmed guard.
        for emitted in [r#"{"decision":"block"}"#, r#"{"decision":"block","reason":"  "}"#] {
            assert!(
                matches!(verdict(emitted), Verdict::Rejected(_)),
                "{emitted} must be rejected"
            );
        }
    }

    #[test]
    fn deny_is_not_a_synonym_for_block_on_the_decision_key() {
        // Easy to assume, and wrong: `decision:deny` is refused even with a
        // reason, while `permissionDecision:deny` is accepted.
        assert!(matches!(
            verdict(r#"{"decision":"deny","reason":"nope"}"#),
            Verdict::Rejected(_)
        ));
        assert_eq!(
            verdict(
                r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"nope"}}"#
            ),
            Verdict::Block("nope".to_owned())
        );
        assert!(matches!(
            verdict(r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#),
            Verdict::Rejected(_)
        ));
    }

    #[test]
    fn keys_codex_names_as_unsupported_are_rejected() {
        for emitted in [
            r#"{"suppressOutput":true}"#,
            r#"{"stopReason":"x"}"#,
            r#"{"continue":false}"#,
            r#"{"reason":"orphaned"}"#,
        ] {
            assert!(
                matches!(verdict(emitted), Verdict::Rejected(_)),
                "{emitted} must be rejected"
            );
        }
        // `continue` is refused on its false value alone: `true` measured as
        // accepted, so the key itself is not unsupported.
        assert_eq!(verdict(r#"{"continue":true}"#), Verdict::Allow);
    }

    #[test]
    fn non_json_is_rejected() {
        assert!(matches!(verdict("not json"), Verdict::Rejected(_)));
        assert!(matches!(verdict("[1,2]"), Verdict::Rejected(_)));
    }

    /// The model above is a description of a specific CLI. If Codex changes
    /// its accepted vocabulary, the description goes stale silently and Boss
    /// resumes shipping a fail-open — exactly the failure mode that produced
    /// this module. Always cross-check against the Bazel-supplied pinned
    /// binary; missing or unreadable test data is a failure.
    #[test]
    fn the_model_matches_the_shipping_binarys_own_rejection_list() {
        let binary = super::super::resolve_codex_bin();
        let bytes = std::fs::read(&binary).expect("Bazel pinned Codex binary must be readable");
        // Lossy decoding preserves these ASCII literals and lets us use the
        // standard library's efficient substring search. Repeated byte-window
        // scans exceed the test deadline in unoptimized builds.
        let text = String::from_utf8_lossy(&bytes);
        let missing: Vec<&str> = BINARY_REJECTION_STRINGS
            .iter()
            .copied()
            .filter(|needle| !text.contains(needle))
            .collect();
        assert!(
            missing.is_empty(),
            "the codex at {} no longer carries {missing:?} — the contract in this module \
             was measured against a different CLI and must be re-measured before it is trusted",
            binary.display()
        );
    }
}
