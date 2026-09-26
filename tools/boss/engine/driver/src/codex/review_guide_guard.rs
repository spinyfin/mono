//! One-command submission allowlist for review-guide workers.
use super::guard_python::with_command_tokenizer;

/// Shared by Codex materialization and Claude settings hooks.
pub fn codex_review_guide_guard_script() -> String {
    with_command_tokenizer(SCRIPT_TEMPLATE).replace("# REVIEW_GUIDE_COMMAND_FRAGMENT", REVIEW_GUIDE_COMMAND_PY)
}

const SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env python3
import json
import os
import re
import shlex
import sys

# COMMAND_TOKENIZER_FRAGMENT

# REVIEW_GUIDE_COMMAND_FRAGMENT

try:
    payload = json.load(sys.stdin)
except Exception:
    payload = None
if allowed(payload):
    print(json.dumps({"decision": "approve"}))
else:
    tool = payload.get("tool_name", "(unreadable payload)") if isinstance(payload, dict) else "(unreadable payload)"
    print(json.dumps({"decision": "block", "reason": "Blocked: review-guide workers may only submit with boss propose review-guide --body followed by a single-quoted Markdown literal; all other tools and commands are forbidden (matched tool: " + str(tool) + ")."}))
"#;

pub(super) const REVIEW_GUIDE_COMMAND_PY: &str = r#"
def allowed(payload):
    if not isinstance(payload, dict) or payload.get("tool_name") != "Bash":
        return False
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        return False
    command = tool_input.get("command")
    if not isinstance(command, str):
        return False
    # Require a literal single-quoted Markdown body (including the standard
    # shell apostrophe escape). No expansion, redirects, wrappers, or chaining.
    literal = r"'(?:[^']|'\"'\"'|'\\'')*'"
    match = re.fullmatch(r"([\s\S]*?)[ \t]+--body[ \t]+(" + literal + r")[ \t\r\n]*", command)
    if not match:
        return False
    prefix = match.group(1)
    if prefix not in ('boss propose review-guide', '"$BOSS_BIN" propose review-guide', '"${BOSS_BIN}" propose review-guide'):
        return False
    # Tokenize the executable/verb shape through the shared guard tokenizer;
    # the separately proven literal body may contain newlines or shell examples.
    groups = command_groups(prefix + " --body 'literal'")
    return len(groups) == 1 and groups[0][1:] == ["propose", "review-guide", "--body", "literal"]

"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn permits_only_literal_guide_submission() {
        for command in [
            "boss propose review-guide --body '# Guide\n## Problem\nA `code` example: $(literal).'",
            "\"$BOSS_BIN\" propose review-guide --body 'author'\"'\"'s guide'",
            "boss propose review-guide --body 'author'\\''s guide'",
        ] {
            let payload = serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}});
            assert_eq!(decide(payload).0, "approve", "{command}");
        }
        for command in [
            "boss propose review-guide --body \"$(cat secret)\"",
            "boss propose review-guide --body 'x'; touch /tmp/x",
            "boss propose review-guide --body 'x' > /tmp/x",
            "boss propose review-guide --body-file /tmp/x",
            "boss propose done --outcome delivered --summary x",
            "env BOSS_RUN_ID=other boss propose review-guide --body 'x'",
            "bash -c \"boss propose review-guide --body 'x'\"",
            "boss propose review-guide --body 'x' && boss propose review-guide --body 'y'",
        ] {
            assert_eq!(
                decide(serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}})).0,
                "block",
                "{command}"
            );
        }
    }

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
    fn blocks_other_tools_and_commands() {
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
            assert!(reason.contains("only submit"), "{reason}");
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
