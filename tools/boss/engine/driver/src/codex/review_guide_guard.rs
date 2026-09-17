//! Codex-only `PreToolUse` guard enforcing the review-guide worker's
//! no-tool-use mandate.
//!
//! A review-guide job ([`crate::WorkerKind::ReviewGuide`]) has exactly one
//! legitimate action: read the immutable source packet the engine already
//! embedded in its prompt, then answer with the finished Markdown guide as
//! ordinary assistant prose. It never needs a tool call to do that — there is
//! no leased implementation checkout to inspect (unlike
//! [`crate::WorkerKind::Reviewer`], which reads a real PR diff and workspace),
//! no file to write (the engine collects the guide from transcript text, not
//! an artifact path), and no publish/reply command to invoke (unlike
//! [`crate::WorkerKind::AnswerAgent`]'s single allowlisted
//! `boss comment reply`).
//!
//! So, unlike every other Codex guard in this module (which parses a shell
//! command or tool name to decide what to block), this one is a flat
//! deny-by-default allowlist with an EMPTY allowlist: every `PreToolUse` call
//! is blocked, regardless of tool name or arguments. This is strictly more
//! conservative than [`super::reviewer_publish_guard`] (which still approves
//! read-only `Bash`/`apply_patch` misses) or
//! [`super::tool_surface_guard`] (which only closes the MCP/`write_stdin`
//! gaps) — neither alone would stop a review-guide worker from reading
//! arbitrary files in its leased workspace, which is not itself a mutation
//! but does breach the "only ever reads the pinned packet" invariant the
//! design requires.
//!
//! Armed only when `ToolUseInterceptionConfig::is_review_guide` is set (see
//! `materialize_guards`), so no other worker kind is affected.

/// The Codex review-guide no-tool-use guard, materialised verbatim as an
/// executable `.py`. Emits a Claude-compatible `{"decision": …}` object on
/// stdout. Matcher `.*` (armed in `materialize_guards`): it must see every
/// tool name, not just `Bash`.
const SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env python3
"""Codex review-guide no-tool-use PreToolUse gate (Boss).

A review-guide job's entire job is to read its prompt (which already embeds
the immutable source packet) and answer with Markdown prose. It never needs a
tool call, so every tool call is blocked outright -- an empty allowlist,
enforced the way Codex enforces anything: a PreToolUse decision, not a
declarative rule list.
"""
import json
import sys

BLOCK = (
    "Blocked: review-guide workers must not call any tool (matched tool: {tool}). "
    "Read the source material already embedded in your prompt and answer with "
    "the finished Markdown guide as your response text -- no tool call is ever "
    "needed or permitted for this job."
)

try:
    payload = json.load(sys.stdin)
except Exception:
    payload = None

tool = payload.get("tool_name") if isinstance(payload, dict) else None
if not isinstance(tool, str) or not tool:
    tool = "(unreadable payload)"

print(json.dumps({"decision": "block", "reason": BLOCK.format(tool=tool)}))
sys.exit(0)
"#;

/// Render the review-guide no-tool-use guard.
pub fn codex_review_guide_guard_script() -> String {
    SCRIPT_TEMPLATE.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn decide(payload: serde_json::Value) -> (String, String) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("boss-codex-review-guide-{0}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, codex_review_guide_guard_script()).unwrap();
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        (
            output["decision"].as_str().unwrap().to_owned(),
            output["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    #[test]
    fn blocks_every_tool_call_regardless_of_shape() {
        for payload in [
            serde_json::json!({"tool_name": "apply_patch", "tool_input": {}}),
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "jj log"}}),
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "cat README.md"}}),
            serde_json::json!({"tool_name": "view_image", "tool_input": {}}),
            serde_json::json!({"tool_name": "mcp__codex_apps__github__get_user_login", "tool_input": {}}),
            serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": "/tmp/x"}}),
        ] {
            let (decision, reason) = decide(payload.clone());
            assert_eq!(decision, "block", "{payload:?} must be blocked, got reason={reason}");
            assert!(reason.contains("must not call any tool"), "{reason}");
        }
    }

    #[test]
    fn blocks_and_names_the_tool_in_the_reason() {
        let (decision, reason) = decide(serde_json::json!({"tool_name": "apply_patch", "tool_input": {}}));
        assert_eq!(decision, "block");
        assert!(reason.contains("apply_patch"), "{reason}");
    }

    #[test]
    fn fails_closed_on_unreadable_payload() {
        let dir = std::env::temp_dir().join(format!("boss-codex-review-guide-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, codex_review_guide_guard_script()).unwrap();
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("python3 must be available");
        child.stdin.as_mut().unwrap().write_all(b"not json").unwrap();
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        let output = serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        assert_eq!(output["decision"].as_str().unwrap(), "block");
    }
}
