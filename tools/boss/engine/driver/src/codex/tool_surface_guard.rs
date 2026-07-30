//! Codex-only `PreToolUse` guard for the parts of Codex's tool surface that
//! Boss's shared (Claude-authored) guards structurally cannot see.
//!
//! # Why a Codex-specific guard exists
//!
//! Boss's other guards inspect a *shell command string*. On Codex that is a
//! real channel — a code-mode cell's `tools.exec_command` call reaches
//! `PreToolUse` normalised as `tool_name: "Bash"` with `tool_input.command`,
//! exactly Claude's shape, so those guards fire and their `block` decisions
//! are enforced pre-execution (verified live on `gpt-5.6-terra`; see
//! `tools/boss/docs/investigations/codex-pretooluse-guard-coverage-2026-07-29.md`).
//!
//! But that is not Codex's *whole* execution surface, and the two gaps are
//! not reachable by any change to a command matcher:
//!
//! 1. **`tools.write_stdin` fires no hook at all.** A cell may start a
//!    long-lived process with `exec_command` (guards see and approve, say,
//!    `sh -s`) and then feed it arbitrary command lines with `write_stdin`,
//!    for which Codex emits **no** `PreToolUse` event. Confirmed by running
//!    `jj git push --dry-run` that way with the PR-redirect guard armed: the
//!    push ran, and the guard was never consulted. There is no payload to
//!    match, so the only hookable point is the *session start* — hence this
//!    guard blocks invocations whose whole purpose is to open a
//!    stdin-driven command channel.
//! 2. **Codex app/MCP tools bypass the shell entirely.** A cell can call
//!    `tools.mcp__codex_apps__github_create_pull_request` (or
//!    `github_create_branch`, `github_update_ref`,
//!    `github_merge_pull_request`, …) authenticated as the operator's
//!    GitHub account. Those *do* fire `PreToolUse`, but with
//!    `tool_name: "mcp__codex_apps__github__create_pull_request"` — which no
//!    `matcher = "Bash"` guard sees. Boss injects no MCP tooling of its own
//!    (`Capability::ToolProvisioning` is deliberately undeclared), so every
//!    `mcp__*` call is out-of-band by construction and is denied.
//!
//! Both blocks keep the worker prompt's assertion — "a PreToolUse hook blocks
//! direct push/PR-create attempts and redirects you to cube" — true rather
//! than aspirational.
//!
//! # Shape
//!
//! Materialised as a real `.py` under `$CODEX_HOME/guards/` and armed with
//! `matcher = ".*"` (it must see non-`Bash` tool names). Unlike the shared
//! guards in [`crate::claude`] this is plain Python source, not a
//! `python3 -c "…"` string: nothing shell-quotes it, so it can use ordinary
//! quoting.
//!
//! Fails **closed**: a payload it cannot read is blocked, never approved.

/// The Codex tool-surface guard, materialised verbatim as an executable
/// `.py`. Emits a Claude-compatible `{"decision": …}` object on stdout.
pub const CODEX_TOOL_SURFACE_GUARD_SCRIPT: &str = r#"#!/usr/bin/env python3
"""Codex tool-surface PreToolUse gate (Boss).

Denies the two Codex execution routes Boss's command-inspecting guards cannot
observe:

  * any `mcp__*` app/MCP tool call -- these do not go through the shell, so no
    command guard sees them, and Boss injects no MCP tooling of its own;
  * any Bash invocation whose only effect is to open a stdin-driven command
    channel (a bare interpreter, `sh -s`, `-i`, an editor/pager), because
    Codex's `tools.write_stdin` writes into such a session with no PreToolUse
    event of its own.

Fails closed: an unreadable payload is blocked, never approved.
"""
import json
import os
import re
import shlex
import sys

MALFORMED = (
    "Blocked (fail-closed): the Boss Codex tool-surface guard could not read "
    "this tool call's payload, so it cannot prove the call is allowed. Guards "
    "deny what they cannot parse rather than approving by default. Report this "
    "payload shape to the operator -- it means Boss guard wiring needs "
    "updating for this Codex version. Detail: "
)

MCP_BLOCK = (
    "Blocked: Boss workers must not act through Codex app/MCP tools (matched "
    "tool: {tool}). Those calls do not go through the shell, so none of Boss's "
    "command guardrails -- push/PR redirection, the Boss-launch gate, the "
    "checkleft pre-push gate, the data-directory gate -- can see them, and a "
    "GitHub app tool would push branches, open or merge PRs outside every one "
    "of those gates. Do the work through the shell instead, where the "
    "guardrails apply: `gh` for GitHub reads, `cube pr create` / "
    "`cube pr update` for pull requests, `jj` for VCS."
)

STDIN_CHANNEL_BLOCK = (
    "Blocked: this starts an interactive, stdin-driven session ({detail}), and "
    "Boss cannot guard what is typed into one. Codex's `write_stdin` feeds an "
    "already-running process new command lines without emitting any PreToolUse "
    "event, so every command sent that way would run unguarded -- pushes and "
    "PR creation included. Run each command as its own shell invocation "
    "instead, so the guards can see it: `bash -lc '<command>'`, "
    "`python3 -c '<code>'`, `python3 <script.py>`, `sqlite3 <db> '<sql>'`. For "
    "editors and pagers, use a non-interactive equivalent (`cat`, `sed`, the "
    "file-editing tool)."
)

# Shells. Separated from the other interpreters because their inline-program
# flag is `-c` and it also appears inside short-flag clusters (`-lc`, `-ic`).
SHELLS = {"sh", "bash", "zsh", "dash", "ksh", "fish", "csh", "tcsh", "ash"}

# Interpreters that read commands from stdin when handed no inline program.
# Blocked only when the invocation carries no program/script to run.
INTERPRETERS = SHELLS | {
    "python", "python2", "python3", "node", "deno", "bun", "ruby", "irb",
    "perl", "php", "lua", "julia", "R", "ghci", "ipython", "bc", "gdb", "lldb",
}

# Interpreters that need two non-flag arguments before they stop being a REPL
# (the first is the resource to open, e.g. a database).
INTERPRETERS_NEEDING_TWO_ARGS = {"sqlite3", "psql", "mysql"}

# Always a live terminal session with a shell escape (`!cmd`, `:!cmd`).
ALWAYS_INTERACTIVE = {
    "vi", "vim", "nvim", "nano", "pico", "emacs", "ed", "less", "more", "man",
    "top", "htop", "btop", "tmux", "screen", "ftp", "telnet", "nc", "ncat",
}

# Flags that make an interpreter read stdin even when other arguments exist.
STDIN_FLAGS = {"-i", "-s", "-", "--interactive", "--stdin"}

# Flags that carry an inline program, *per interpreter*. One shared set across
# every interpreter was a hole: `-e`, `-E` and `-m` are ordinary shell options
# that carry no program at all, so `bash -e`, `sh -E` and `zsh -m` -- each a
# bare interpreter reading commands from stdin, exactly what `write_stdin` then
# drives -- read as "has a program" and were approved. Every flag here needs an
# operand (either a following argument or a `--flag=value` form); a flag with
# no operand does not make the invocation a program run. A flag NOT listed for
# an interpreter is not treated as carrying a program, so the REPL check below
# still applies to it.
INLINE_PROGRAM_FLAGS = {
    "python": {"-c", "-m"},
    "python2": {"-c", "-m"},
    "python3": {"-c", "-m"},
    "ipython": {"-c", "-m"},
    "node": {"-e", "-E", "--eval", "-p", "--print"},
    "deno": {"-e", "--eval"},
    "bun": {"-e", "--eval"},
    "ruby": {"-e", "--eval"},
    "irb": {"-e"},
    "perl": {"-e", "-E", "--eval"},
    "php": {"-r", "-f"},
    "lua": {"-e"},
    "julia": {"-e", "-E"},
    "R": {"-e"},
    "ghci": {"-e"},
    "gdb": {"-ex", "--eval-command", "-init-command", "--init-eval-command", "-x"},
    "lldb": {"-o", "--one-line", "--source"},
    "sqlite3": {"-cmd"},
    "psql": {"-c", "--command", "-f", "--file"},
    "mysql": {"-e", "--execute"},
    # `bc` has no inline-program flag: it is a REPL or it reads a file.
    "bc": set(),
}

# A short-flag cluster such as `-lc` or `-ic`, as shells accept it.
SHELL_FLAG_CLUSTER = re.compile(r"^-[A-Za-z]+$")

# Shell delimiters that separate independent commands.
DELIMS = {"&&", "||", ";", "|", "&"}

# Launcher prefixes that wrap the real program.
WRAPPERS = {
    "nohup", "env", "sudo", "exec", "command", "stdbuf", "setsid",
    "caffeinate", "xargs", "time",
}

ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def command_groups(command):
    """Split a command string into independent argv groups."""
    groups = []
    for line in command.split("\n"):
        try:
            tokens = shlex.split(line, posix=True)
        except Exception:
            tokens = line.split()
        current = []
        for token in tokens:
            if token in DELIMS:
                if current:
                    groups.append(current)
                current = []
            else:
                current.append(token)
        if current:
            groups.append(current)
    return groups


def strip_prefixes(group):
    """Drop env assignments and launcher wrappers to reach the real program."""
    index = 0
    while index < len(group):
        base = os.path.basename(group[index])
        if ASSIGNMENT.match(group[index]) or base in WRAPPERS:
            index += 1
            continue
        if base == "timeout" and index + 1 < len(group):
            index += 2
            continue
        break
    return group[index:]


def has_inline_program(program, arguments):
    """True when a flag *this interpreter* recognises carries a program to run.

    Requires an operand: `python3 -m mod` and `node --eval=x` carry a program,
    `python3 -m` alone does not (it is still a REPL reading stdin).
    """
    if program in SHELLS:
        for index, argument in enumerate(arguments):
            cluster = SHELL_FLAG_CLUSTER.match(argument) and "c" in argument[1:]
            if (argument == "-c" or cluster) and index + 1 < len(arguments):
                return True
        return False

    flags = INLINE_PROGRAM_FLAGS.get(program)
    if not flags:
        return False
    for index, argument in enumerate(arguments):
        if argument in flags:
            if index + 1 < len(arguments):
                return True
            continue
        head, separator, value = argument.partition("=")
        if separator and value and head in flags:
            return True
    return False


def stdin_channel_detail(group):
    """Why this argv opens a stdin-driven session, or None if it does not."""
    rest = strip_prefixes(group)
    if not rest:
        return None
    program = os.path.basename(rest[0])
    arguments = rest[1:]

    if program in ALWAYS_INTERACTIVE:
        return "%s is an interactive terminal program with a shell escape" % program

    interpreter = program in INTERPRETERS or program in INTERPRETERS_NEEDING_TWO_ARGS
    if not interpreter:
        return None

    for argument in arguments:
        if argument in STDIN_FLAGS:
            return "%s %s reads commands from stdin" % (program, argument)

    if has_inline_program(program, arguments):
        return None

    non_flags = [a for a in arguments if not a.startswith("-")]
    required = 2 if program in INTERPRETERS_NEEDING_TWO_ARGS else 1
    if len(non_flags) >= required:
        return None
    return "%s with no program or script to run is a REPL reading stdin" % program


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception as error:
        emit("block", MALFORMED + "hook stdin was not JSON (%s)" % error)
    if not isinstance(payload, dict):
        emit("block", MALFORMED + "hook payload was not a JSON object")

    tool = payload.get("tool_name")
    if not isinstance(tool, str) or not tool:
        emit("block", MALFORMED + "tool_name was missing or not a string")

    if tool.startswith("mcp__"):
        emit("block", MCP_BLOCK.format(tool=tool))

    if tool != "Bash":
        # Every other Codex tool (apply_patch, view_image, update_plan, the
        # file readers) is either harmless here or covered by another guard.
        emit("approve")

    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        emit("block", MALFORMED + "tool_input was not an object")
    command = tool_input.get("command")
    if not isinstance(command, str):
        emit("block", MALFORMED + "tool_input.command was not a string")

    for group in command_groups(command):
        detail = stdin_channel_detail(group)
        if detail:
            emit("block", STDIN_CHANNEL_BLOCK.format(detail=detail))

    emit("approve")


if __name__ == "__main__":
    main()
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Run the guard against a payload and return its decision.
    fn decide(payload: serde_json::Value) -> (String, String) {
        // One directory per call: these tests run concurrently in-process, and
        // sharing a script path lets one test read a half-written file.
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("boss-codex-tool-surface-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("guard.py");
        std::fs::write(&script, CODEX_TOOL_SURFACE_GUARD_SCRIPT).unwrap();

        let mut child = std::process::Command::new("python3")
            .arg(&script)
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
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
            panic!(
                "guard produced invalid JSON: {err}\nstdout={stdout}\nstderr={}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (
            parsed["decision"].as_str().unwrap_or("missing").to_owned(),
            parsed["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    fn bash(command: &str) -> (String, String) {
        decide(serde_json::json!({"tool_name": "Bash", "tool_input": {"command": command}}))
    }

    #[test]
    fn blocks_every_mcp_tool_call() {
        // The GitHub app tools can push branches and open PRs with no shell
        // command for any command guard to inspect.
        let (decision, reason) = decide(serde_json::json!({
            "tool_name": "mcp__codex_apps__github__create_pull_request",
            "tool_input": {"title": "x"},
        }));
        assert_eq!(decision, "block", "{reason}");
        assert!(
            reason.contains("cube pr create"),
            "reason must redirect to cube: {reason}"
        );
    }

    #[test]
    fn blocks_read_only_mcp_tools_too() {
        // Deny-by-default rather than an allowlist of read-only verbs: the
        // verb set drifts with Codex's app catalog, an allowlist does not.
        let (decision, _) = decide(serde_json::json!({
            "tool_name": "mcp__codex_apps__github__get_user_login",
            "tool_input": {},
        }));
        assert_eq!(decision, "block");
    }

    #[test]
    fn approves_ordinary_codex_tools() {
        for tool in ["apply_patch", "view_image", "update_plan", "web__run"] {
            let (decision, reason) = decide(serde_json::json!({
                "tool_name": tool,
                "tool_input": {"command": "*** Begin Patch"},
            }));
            assert_eq!(decision, "approve", "{tool} must be left to other guards: {reason}");
        }
    }

    #[test]
    fn blocks_bare_interpreters_that_read_stdin() {
        for command in [
            "bash",
            "sh",
            "sh -s",
            "zsh -i",
            "python3",
            "node",
            "/bin/bash",
            "python3 -",
            // Option flags that carry no inline program. Each of these starts
            // an interpreter reading commands from stdin, which is precisely
            // what `write_stdin` then feeds; an interpreter-agnostic
            // inline-program flag set approved all four.
            "bash -e",
            "sh -E",
            "zsh -m",
            "python3 -E",
            // A recognised inline-program flag with no operand is still a REPL.
            "python3 -m",
            "bash -c",
        ] {
            let (decision, reason) = bash(command);
            assert_eq!(decision, "block", "{command:?} opens a stdin command channel");
            assert!(
                reason.contains("write_stdin"),
                "reason must name the write_stdin channel: {reason}"
            );
        }
    }

    #[test]
    fn blocks_stdin_channel_hidden_behind_a_wrapper_or_later_group() {
        for command in ["nohup bash", "FOO=1 sh -s", "echo hi && bash", "timeout 30 python3"] {
            let (decision, _) = bash(command);
            assert_eq!(decision, "block", "{command:?} must still be caught");
        }
    }

    #[test]
    fn blocks_interactive_editors_and_pagers() {
        for command in ["vim notes.txt", "less big.log", "tmux new -s x"] {
            let (decision, _) = bash(command);
            assert_eq!(decision, "block", "{command:?} is a live terminal session");
        }
    }

    #[test]
    fn approves_non_interactive_interpreter_invocations() {
        for command in [
            "bash -lc 'echo hi'",
            // A cluster carrying `c` is an inline program even with `i` in it:
            // the shell runs the command and exits, so there is no session for
            // `write_stdin` to drive.
            "bash -ic 'echo hi'",
            "sh -c 'ls'",
            "bash -e -c 'ls'",
            "python3 -c 'print(1)'",
            "python3 -m pytest",
            "python3 script.py",
            "python3 -E script.py",
            "node -e 'console.log(1)'",
            "node --eval='console.log(1)'",
            "sqlite3 db.sqlite 'select 1'",
            "bazel test //tools/boss/engine/...",
            "jj diff -r @",
            "cube pr create --branch b --title t --body-file f",
        ] {
            let (decision, reason) = bash(command);
            assert_eq!(decision, "approve", "{command:?} must be allowed: {reason}");
        }
    }

    #[test]
    fn sqlite_repl_without_sql_is_blocked() {
        // `sqlite3 db` alone is a REPL; the guard needs two non-flag args.
        let (decision, _) = bash("sqlite3 db.sqlite");
        assert_eq!(decision, "block");
    }

    #[test]
    fn fails_closed_on_unreadable_payloads() {
        // The exact hazard this guard exists to survive: a payload shape Boss
        // did not anticipate must block, not approve.
        let (decision, reason) = decide(serde_json::json!({"tool_name": "Bash", "tool_input": "js source"}));
        assert_eq!(decision, "block", "{reason}");
        assert!(reason.contains("fail-closed"), "{reason}");

        let (decision, _) = decide(serde_json::json!({"tool_input": {"command": "ls"}}));
        assert_eq!(decision, "block", "a payload with no tool_name must block");
    }
}
