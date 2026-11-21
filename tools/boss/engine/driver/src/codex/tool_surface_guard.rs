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
use super::guard_python::with_command_tokenizer;

/// The Codex tool-surface guard, materialised verbatim as an executable
/// `.py`. Emits a Claude-compatible `{"decision": …}` object on stdout.
const SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env python3
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
import stat
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
    "`cube pr update` for pull requests, `jj` for VCS. If you are a "
    "revision: never open a new PR; push in-scope work with "
    "`\"$CUBE_BIN\" pr update`. If another PR must land before this "
    "revision can finish and your instructions enable completion declarations, run `\"$BOSS_BIN\" propose done --outcome blocked "
    "--summary \"<blocker>\" --answer-type create-prerequisite-task --prerequisite-name \"<short "
    "task name>\" --prerequisite-brief \"<self-contained description>\" "
    "--explanation-file /tmp/explanation.txt`; otherwise report the dependency using the blocker channel in your instructions. `\"$BOSS_BIN\" propose followup-task --name \"<short> "
    "title>\" --description \"<one paragraph of scope>\" --rationale "
    "\"<why a separate PR>\"` is only for non-blocking work that can happen "
    "later; finish the in-scope work."
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

# COMMAND_TOKENIZER_FRAGMENT
# REVIEW_GUIDE_COMMAND_FRAGMENT


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


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


def stdin_comes_from_redirect(group, cwd):
    """Accept one fixed fd-0 source, preserving quote provenance.

    Adjacent operators inside shlex words fail closed: they are shell syntax,
    while quoted angle brackets remain ordinary filename characters.
    """
    redirects = []
    for index, token in enumerate(group):
        literal = group.literal_chars[index]
        angles = [i for i, char in enumerate(token) if char in '<>' and not literal[i]]
        if not angles:
            continue
        match = re.match(r"([0-9]*)(&>>|&>|<<<|<<-|<<|<>|[<>]&|>>|[<>])", token)
        if not match or any(literal[:match.end()]):
            return False
        fd, operator = match.groups()
        if any(i >= match.end() for i in angles):
            return False
        fd = int(fd) if fd else (0 if operator.startswith('<') else 1)
        if fd != 0:
            continue
        if operator not in ('<', '<<', '<<-', '<<<'):
            return False
        target = token[match.end():] or (group[index + 1] if index + 1 < len(group) else '')
        redirects.append((operator, target))
    if len(redirects) != 1:
        return False
    operator, target = redirects[0]
    if operator != '<':
        return bool(target)
    # Expansions and preceding directory changes prevent proving which file
    # will be opened. Resolve symlinks before checking device roots and type.
    if not target or any(char in target for char in '$~*?[]`'):
        return False
    if not os.path.isabs(target):
        if cwd is None:
            return False
        target = os.path.join(cwd, target)
    try:
        resolved = os.path.realpath(target)
        if any(resolved == root or resolved.startswith(root + '/')
               for root in ('/dev', '/private/dev', '/proc')):
            return False
        try:
            return stat.S_ISREG(os.stat(resolved).st_mode)
        except FileNotFoundError:
            # A missing file cannot supply live stdin; the shell fails open().
            return True
    except (OSError, ValueError):
        return False


def stdin_channel_detail(group, cwd):
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

    # `python3 - <<'PY'` and `sqlite3 db < script.sql` read a heredoc or file,
    # not a live session. A shell is excluded: the body it would read is a
    # script the other guards cannot attribute to a command of its own.
    if program not in SHELLS and stdin_comes_from_redirect(group, cwd):
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

    if payload.get("redirects_stripped") is True:
        # The adapter also offers each command with its redirections removed,
        # so a prefix guard can see the program. This guard reads redirections
        # itself, from the unstripped form it is also given; the stripped form
        # would only lose a heredoc or file that is the program's stdin.
        emit("approve")

    masked = review_guide_masked_command(command)
    if masked is not None:
        command = masked
    cwd = payload.get("cwd", tool_input.get("cwd", tool_input.get("workdir", os.getcwd())))
    if not isinstance(cwd, str) or not os.path.isabs(cwd):
        cwd = None
    for group in command_groups(command):
        rest = strip_prefixes(group)
        if rest and os.path.basename(rest[0]) in ('cd', 'pushd', 'popd'):
            cwd = None
        detail = stdin_channel_detail(group, cwd)
        if detail:
            emit("block", STDIN_CHANNEL_BLOCK.format(detail=detail))

    emit("approve")


if __name__ == "__main__":
    main()
"#;

/// Render the tool-surface guard with the shared shell-command tokenizer.
pub fn codex_tool_surface_guard_script() -> String {
    with_command_tokenizer(SCRIPT_TEMPLATE).replace(
        "# REVIEW_GUIDE_COMMAND_FRAGMENT",
        super::review_guide_guard::REVIEW_GUIDE_COMMAND_PY,
    )
}

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
        std::fs::write(&script, codex_tool_surface_guard_script()).unwrap();

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
    fn guide_submission_literal_is_data_even_when_it_contains_shell_examples() {
        let (decision, reason) = bash(
            "\"$BOSS_BIN\" propose review-guide --body '# Guide\nExample:\npython3\nbash\nswift run\nboss engine start\n$(not executed)\n'",
        );
        assert_eq!(decision, "approve", "{reason}");
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
        assert!(
            reason.contains("--summary \"<blocker>\" --answer-type create-prerequisite-task")
                && reason.contains("instructions enable completion declarations")
                && reason.contains("otherwise report the dependency")
                && reason.contains("propose followup-task --name"),
            "reason must name create-prerequisite-task for a PR that must land first: {reason}"
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
    fn blocks_stdin_channel_after_an_unspaced_operator() {
        // An operator glued to the preceding word (no surrounding
        // whitespace) must still split the group, so the interactive
        // program after it is seen instead of being fused into one token.
        for command in ["echo hi&&bash", "echo hi;python3", "echo a#b && bash"] {
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
    fn approves_interpreters_whose_stdin_is_a_heredoc_or_file() {
        for command in [
            "python3 - <<'PY'\nprint(1)\nPY",
            "python3 - <<PY\nprint(1)\nPY",
            "python3 - <<-'PY'\nprint(1)\nPY",
            "python3 <<'PY'\nprint(1)\nPY",
            "python3 - < script.py",
            "python3 - < 'script<name.py'",
            "python3 - < script.py 2>&1",
            "python3 - < script.py >&2",
            "python3 - < script.py &>/dev/null",
            "python3 - <<'PY' &>>log\nprint(1)\nPY",
            "python3 - <<'PY' 2>&1\nprint(1)\nPY",
            "node - <<'JS' >&2\nconsole.log(1)\nJS",
            "python3 - < script.py 2>/dev/null",
            "python3 - <<<'print(1)'",
            "sqlite3 db.sqlite < script.sql",
            "node - <<'JS'\nconsole.log(1)\nJS",
            "python3 - 2>/dev/null <<'PY'\nprint(1)\nPY",
        ] {
            let (decision, reason) = bash(command);
            assert_eq!(decision, "approve", "{command:?} reads a heredoc or file: {reason}");
        }
    }

    #[test]
    fn a_quoted_or_non_input_redirect_does_not_hide_a_stdin_channel() {
        for command in [
            // Quoted: an ordinary argument that merely starts with `<`.
            "python3 - '<x'",
            "python3 - \"<<PY\"",
            "python3 - \\<x",
            // Not a file or heredoc feeding stdin.
            "python3 - <&3",
            "python3 - <script.py</dev/tty",
            "python3 - < script.py</dev/tty",
            "python3 - < script.py 0>&1",
            "python3 - < script.py 0>out",
            "python3 - < script.py < /dev/tty",
            "python3 - 3<&0 < script.py <&3",
            "python3 - 3>&0 < script.py 0>&3",
            "python3 - <<'PY' < /dev/tty\nprint(1)\nPY",
            "python3 - <>f",
            "python3 - >out",
            // A terminal or process device is a live channel, not a file.
            "python3 - < /dev/tty",
            "python3 - < //dev/tty",
            "python3 - < /./dev/tty",
            "python3 - < /private/dev/tty",
            "python3 - < ../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../../dev/tty",
            "cd /dev && python3 - < tty",
            "python3 - < $INPUT",
            "python3 - < ~/input",
            "python3 - < *.py",
            "python3 - < script.py &>log</dev/tty",
            "python3 - <<'PY' &>>log</dev/tty\nprint(1)\nPY",
            "python3 -</dev/stdin",
            "python3 - 0< /proc/self/fd/0",
            // A shell reading a heredoc stays blocked.
            "bash <<'EOF'\necho hi\nEOF",
            "sh -s <<'EOF'\necho hi\nEOF",
            // The redirect belongs to another command.
            "echo x <<'EOF' && python3 -\nbody\nEOF",
            "cat < in.txt | python3 -",
        ] {
            let (decision, reason) = bash(command);
            assert_eq!(decision, "block", "{command:?} still opens a stdin channel: {reason}");
        }
    }

    #[test]
    fn redirect_sources_resolve_against_payload_cwd_and_reject_special_files() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/dev/tty", dir.path().join("terminal")).unwrap();
        std::fs::write(dir.path().join("regular.py"), "print(1)\n").unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(dir.path().join("fifo"))
                .status()
                .unwrap()
                .success()
        );
        for (target, expected) in [
            ("terminal", "block"),
            ("fifo", "block"),
            (".", "block"),
            ("regular.py", "approve"),
        ] {
            let (decision, reason) = decide(serde_json::json!({
                "tool_name": "Bash", "cwd": dir.path(),
                "tool_input": {"command": format!("python3 - < {target}")}
            }));
            assert_eq!(decision, expected, "{target}: {reason}");
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
