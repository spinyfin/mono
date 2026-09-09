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
//!   mutations, and `cube pr create`/`update`/`ensure`.
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
pub const CODEX_REVIEWER_PUBLISH_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
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

DELIMS = {"&&", "||", ";", "|", "&"}

ASSIGNMENT_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def command_groups(cmd):
    try:
        toks = shlex.split(cmd, posix=True)
    except Exception:
        toks = cmd.split()
    groups = []
    cur = []
    for t in toks:
        if t in DELIMS:
            if cur:
                groups.append(cur)
            cur = []
        else:
            cur.append(t)
    if cur:
        groups.append(cur)
    return groups


def matched_publish_command(cmd):
    dol = chr(36)
    for group in command_groups(cmd):
        i = 0
        while i < len(group) and ASSIGNMENT_RE.match(group[i]):
            i += 1
        rest = group[i:]
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
        if len(rest) >= 3 and is_cube and rest[1] == "pr" and rest[2] in (
            "create", "update", "ensure",
        ):
            return "cube pr " + rest[2]
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
