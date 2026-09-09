//! Codex-only `PreToolUse` guard enforcing the reviewer read-only /
//! no-publish mandate.
//!
//! Claude and local Grok reviewers get this fence from a declarative
//! `permissions.deny` allowlist
//! (`boss_engine_core::worker_setup::reviewer_deny_rules`) plus their
//! driver's OS sandbox. Codex has no equivalent declarative deny surface,
//! and — since the reviewer OS sandbox was removed (it desynced the
//! session's real working directory from the one hooks were armed and
//! trust-attested against, making every Codex reviewer appear
//! never-started) — no OS sandbox either. Without a replacement, a Codex
//! reviewer could edit the checkout or push/publish with nothing to stop
//! it, unlike its Claude and Grok counterparts.
//!
//! This guard is that replacement, expressed the way Codex enforces
//! anything: a `PreToolUse` decision, not a declarative rule list.
//!
//! - Every `apply_patch` call is blocked outright. A reviewer never
//!   legitimately writes a file: its `ReviewResult` is extracted from
//!   rollout/transcript text, not written by the model
//!   (`CodexDriver::structured_output_fallback`), so there is no
//!   sanctioned write for this guard to carve an exception for (contrast
//!   Claude's `reviewer_deny_rules`, which scopes its file-write deny
//!   because the Claude reviewer *does* write one engine-owned artifact).
//! - The same publish commands `publish_deny_rules` denies for Claude/Grok
//!   are blocked here too: `jj git push` / `git push`, `gh pr`/`gh issue`
//!   mutations, and all `cube pr` subcommands.
//!
//! Armed only when `ToolUseInterceptionConfig::is_reviewer` is set (see
//! `materialize_guards`), so no other worker kind is affected.

/// The Codex reviewer publish/no-write guard, materialised verbatim as an
/// executable `.py`. Emits a Claude-compatible `{"decision": …}` object on
/// stdout. Matcher `.*` (armed in `materialize_guards`): it must see
/// `apply_patch`, not just `Bash`.
///
/// Approves silently on every tool call it has nothing to say about (`Read`,
/// `Grep`, plain `Bash` reads, …) — only `apply_patch` and a `Bash` command
/// matching a publish shape are blocked.
use super::guard_python::with_command_tokenizer;

const SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env python3
"""Codex reviewer read-only / no-publish PreToolUse gate (Boss).

Replaces, for Codex, the two controls the Claude/Grok reviewer gets from
declarative `permissions.deny` rules (`reviewer_deny_rules` in
`boss_engine_core::worker_setup`): no file writes, and no push/publish.
Fails closed: an unreadable payload for a tool this guard must judge is
blocked, never approved.
"""
import json
import os
import re
import shlex
import sys

MALFORMED = (
    "Blocked (fail-closed): the Boss reviewer no-write/no-publish guard "
    "could not read this tool call's payload, so it cannot prove the call "
    "is allowed. Guards deny what they cannot parse rather than approving "
    "by default. Report this payload shape to the operator -- it means "
    "Boss guard wiring needs updating for this Codex version."
)

WRITE_BLOCK = (
    "Blocked: reviewer workers are read-only and must not modify any file "
    "(matched tool: apply_patch). Record findings in the review result "
    "instead of editing the checkout."
)

PUBLISH_BLOCK = (
    "Blocked: reviewer workers must not push branches or write to GitHub "
    "(matched command: {matched}). Reviewers report findings; they never "
    "publish."
)

# COMMAND_TOKENIZER_FRAGMENT


def matched_publish_command(cmd):
    dol = chr(36)
    for group in command_groups(cmd):
        rest = strip_prefixes(group)
        if not rest:
            continue
        prog = os.path.basename(rest[0])
        is_cube = prog == "cube" or rest[0] in (dol + "CUBE_BIN", dol + "{CUBE_BIN}")
        if len(rest) >= 3 and prog == "jj" and rest[1] == "git" and rest[2] == "push":
            return "jj git push"
        if len(rest) >= 2 and prog == "git" and rest[1] == "push":
            return "git push"
        if len(rest) >= 3 and prog == "gh" and rest[1] == "pr" and rest[2] in (
            "create", "merge", "close", "edit", "comment", "review",
        ):
            return "gh pr " + rest[2]
        if len(rest) >= 3 and prog == "gh" and rest[1] == "issue" and rest[2] in (
            "create", "comment", "close", "edit",
        ):
            return "gh issue " + rest[2]
        if len(rest) >= 2 and is_cube and rest[1] == "pr":
            return "cube pr"
    return None


def emit(decision):
    print(json.dumps(decision))
    sys.exit(0)


try:
    payload = json.load(sys.stdin)
except Exception:
    emit({"decision": "block", "reason": MALFORMED})

if not isinstance(payload, dict):
    emit({"decision": "block", "reason": MALFORMED})

tool_name = payload.get("tool_name")

if tool_name == "apply_patch":
    emit({"decision": "block", "reason": WRITE_BLOCK})

if tool_name != "Bash":
    emit({"decision": "approve"})

tool_input = payload.get("tool_input")
if not isinstance(tool_input, dict):
    emit({"decision": "block", "reason": MALFORMED})

cmd = tool_input.get("command")
if not isinstance(cmd, str):
    emit({"decision": "block", "reason": MALFORMED})

matched = matched_publish_command(cmd)
if matched:
    emit({"decision": "block", "reason": PUBLISH_BLOCK.format(matched=matched)})

emit({"decision": "approve"})
"#;

/// Render the reviewer guard with the shared shell-command tokenizer.
pub fn codex_reviewer_publish_guard_script() -> String {
    with_command_tokenizer(SCRIPT_TEMPLATE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn decide(payload: serde_json::Value) -> (String, String) {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("boss-codex-reviewer-publish-{0}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, codex_reviewer_publish_guard_script()).unwrap();
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

    fn bash(command: &str) -> (String, String) {
        decide(serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}}))
    }

    #[test]
    fn blocks_writes_and_publish_commands() {
        assert_eq!(
            decide(serde_json::json!({"tool_name": "apply_patch", "tool_input": {}})).0,
            "block"
        );
        for command in [
            "jj git push",
            "git push -f origin x",
            "gh pr create",
            "gh issue comment",
            "cube pr update",
            "cube pr arbitrary-verb",
            "git status&&git push",
            "x;git push",
            "git status\ngit push",
            "env git push",
            "sudo git push",
            "timeout 60 gh pr create",
            "nohup git push",
        ] {
            assert_eq!(bash(command).0, "block", "{command:?} must be blocked");
        }
    }

    #[test]
    fn approves_read_only_tools_and_commands() {
        for tool in ["Read", "Grep"] {
            assert_eq!(
                decide(serde_json::json!({"tool_name": tool, "tool_input": {}})).0,
                "approve"
            );
        }
        for command in ["gh pr view", "jj log", "git status"] {
            assert_eq!(bash(command).0, "approve", "{command:?} must be approved");
        }
    }

    #[test]
    fn blocks_malformed_payload() {
        let (decision, reason) = decide(serde_json::json!({"tool_name": "Bash", "tool_input": {}}));
        assert_eq!(decision, "block");
        assert!(reason.contains("fail-closed"));
    }
}
