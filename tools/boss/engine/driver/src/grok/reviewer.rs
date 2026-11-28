//! Reviewer authorization through the Grok PreToolUse adapter.
//!
//! No shell blacklist can make arbitrary programs read-only. Reuse the source
//! inspection grammar and admit only non-mutating VCS/GitHub command forms.
//! The sole file-write exception is this run's engine-owned review result.
use std::path::Path;

use boss_engine_structured_output::{StructuredOutputKind, default_dir, path_for};

pub(super) fn script(workspace: &Path, run_id: &str) -> anyhow::Result<String> {
    anyhow::ensure!(workspace.is_absolute(), "Grok Reviewer requires an absolute workspace");
    let artifact = path_for(&default_dir(), run_id, StructuredOutputKind::ReviewResult);
    let script = crate::codex::with_source_reads(&crate::guard_python::with_command_tokenizer(SCRIPT));
    Ok(script
        .replace("__WORKSPACE__", &serde_json::to_string(workspace)?)
        .replace("__ARTIFACT__", &serde_json::to_string(&artifact)?))
}

const SCRIPT: &str = r#"#!/usr/bin/env python3
import json
import os
import re
import shlex
import sys

# COMMAND_TOKENIZER_FRAGMENT
# REVIEW_GUIDE_READ_FRAGMENT

WORKSPACE = __WORKSPACE__
ARTIFACT = __ARTIFACT__
os.environ["BOSS_REVIEW_GUIDE_WORKSPACE"] = WORKSPACE


def options(args, flags=(), values=()):
    """Reject every unrecognised option, including hidden execution/output flags."""
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--":
            return True
        if arg in flags:
            index += 1
            continue
        if arg in values:
            if index + 1 == len(args):
                return False
            index += 2
            continue
        if arg.startswith("-"):
            return False
        index += 1
    return True


def reviewer_read_stage(tokens, payload, piped):
    if not tokens:
        return False
    prog, args = tokens[0], tokens[1:]
    if prog == "git" and not piped:
        # Disable user-configured pagers; never accept -c, aliases or external
        # diff/textconv helpers. Git object inspection does not update the index.
        if not args or args.pop(0) != "--no-pager":
            return False
        if args[:1] in (["-C"], ["--git-dir"]):
            if len(args) < 3 or not os.path.isabs(args[1]):
                return False
            args = args[2:]
        if not args:
            return False
        sub, args = args[0], args[1:]
        if sub == "show" and len(args) == 1 and ":" in args[0] and not args[0].startswith("-"):
            revision, _, path = args[0].partition(":")
            return bool(re.fullmatch("[0-9a-fA-F]{40}", revision) and guide_object_path(path))
        if sub in ("diff", "show"):
            if args[:2] != ["--no-ext-diff", "--no-textconv"]:
                return False
            if sub == "show":
                if args[2:3] != ["--no-show-signature"]:
                    return False
                args = args[:2] + args[3:]
            # Object IDs cannot resolve to the index or working tree, where
            # even --no-textconv diffs can execute clean/process filters.
            operands = [arg for arg in args[2:] if not arg.startswith("-")]
            required = 2 if sub == "diff" else 1
            return "--" not in args and len(operands) == required and all(
                re.fullmatch("[0-9a-fA-F]{40}", arg) for arg in operands
            ) and options(args[2:], ("--stat", "--name-only", "--name-status", "--numstat",
                                     "--patch", "--raw", "--summary", "--check"))
        if sub == "log" and args[:2] == ["--no-patch", "--no-show-signature"]:
            return options(args[2:], ("--oneline", "--all"), ("-n", "--max-count", "--format"))
        return False
    if prog == "jj" and not piped:
        # Even jj reads snapshot the checkout without --ignore-working-copy.
        remaining, protective = [], set()
        index = 0
        while index < len(args):
            arg = args[index]
            if arg == "--":
                # Flags after the option terminator are only path operands.
                return False
            if arg in ("--ignore-working-copy", "--no-pager"):
                protective.add(arg)
            elif arg in ("-R", "--repository"):
                if index + 1 == len(args) or not os.path.isabs(args[index + 1]):
                    return False
                index += 1
            else:
                remaining.append(arg)
            index += 1
        if protective != {"--ignore-working-copy", "--no-pager"}:
            return False
        args = remaining
        if not args:
            return False
        sub, args = args[0], args[1:]
        if sub in ("diff", "show"):
            # Explicit built-in format prevents configured external diff tools.
            return bool(args) and args[0] == "--git" and options(
                args[1:], (), ("-r", "--revision", "--from", "--to"))
        if sub == "log":
            return options(args, ("--no-graph", "--reversed"), ("-r", "--revisions", "-n", "--limit", "-T", "--template"))
        if sub == "file" and args[:1] in (["show"], ["list"]):
            return options(args[1:], (), ("-r", "--revision"))
        return sub == "status" and not args
    if prog == "gh" and not piped:
        if args[:1] in (["-R"], ["--repo"]):
            if len(args) < 3:
                return False
            args = args[2:]
        if len(args) < 2:
            return False
        sub, verb, args = args[0], args[1], args[2:]
        if sub in ("pr", "issue") and verb in ("view", "list"):
            # No --web (launches a browser), editor, extensions or API writes.
            return options(args, ("--comments",), ("--json", "--jq", "-q", "--state", "--limit", "--base", "--head", "--repo", "-R"))
        if sub == "pr" and verb in ("diff", "checks"):
            return options(args, ("--patch", "--name-only", "--required"), ("--json", "--jq", "--repo", "-R"))
        return False
    if prog == "rg":
        # ripgrep config can inject --pre, an arbitrary executable.
        if args[:1] != ["--no-config"]:
            return False
        tokens = [prog] + args[1:]
    return guide_read_stage(tokens, payload, piped)


def artifact_write(data, cwd):
    path = data.get("file_path")
    if not isinstance(path, str) or not isinstance(data.get("content"), str):
        return False
    candidate = os.path.abspath(os.path.join(cwd, path))
    # Do not let an artifact symlink/hardlink turn the exception into a source edit.
    if os.path.realpath(candidate) != os.path.realpath(ARTIFACT) or os.path.islink(candidate):
        return False
    if os.path.exists(candidate) and (not os.path.isfile(candidate) or os.stat(candidate).st_nlink != 1):
        return False
    return not os.path.islink(ARTIFACT)


def review_report(data):
    # Match the rendered command before the read grammar rejects its one
    # intentional shell expansion. The engine validates batch and target binding.
    command = data.get("command", "")
    if not isinstance(command, str):
        return False
    match = re.fullmatch(
        r'"\$BOSS_BIN" propose review-report --batch-id ([A-Za-z0-9_-]+) '
        r'--target-sha ([0-9a-fA-F]{40}) --body-file "([^"\\$`\n]+)"',
        command,
    )
    return bool(match and os.path.isabs(match[3])
                and os.path.realpath(match[3]) == os.path.realpath(ARTIFACT))


def allowed(payload):
    if not isinstance(payload, dict) or payload.get("toolInputTruncated"):
        return False
    tool = payload.get("tool_name")
    data = payload.get("tool_input")
    cwd = payload.get("cwd", WORKSPACE)
    if not isinstance(data, dict) or not isinstance(cwd, str):
        return False
    if tool in ("read_file", "grep", "list_dir", "glob", "get_command_or_subagent_output", "Read", "Grep", "Glob"):
        return True
    if tool == "Write":
        return artifact_write(data, cwd)
    if tool == "Bash":
        return review_report(data) or guide_shell_read(payload, reviewer_read_stage)
    # New native tools, MCP, subagents and stdin injection must not open an
    # uninspected write path. They require an explicit read-only contract.
    return False


try:
    approved = allowed(json.load(sys.stdin))
except Exception:
    approved = False
print(json.dumps({"decision": "approve"} if approved else {
    "decision": "block",
    "reason": "Grok Reviewer is read-only: edits, shell writes and publishing are denied. "
              "Use native read/search tools or cat/head/tail/sed -n; use rg --no-config. "
              "For Git use full object IDs, --no-pager and diff/show --no-ext-diff --no-textconv; "
              "for jj use --ignore-working-copy --no-pager and diff/show --git. "
              "Only the native write tool may write this run's review-result JSON."
}))
"#;

#[cfg(test)]
#[path = "reviewer_tests.rs"]
mod tests;
