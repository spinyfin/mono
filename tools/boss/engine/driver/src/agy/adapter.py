#!/usr/bin/env python3
"""Antigravity PreToolUse authorization adapter.

One entry point for every tool call: silently forward the raw payload to
boss-event, run the engine-owned guard chain, then print exactly one
verdict (auto_approve, deny, or deny_unless_prior_grant). Silent stdout
is not an authorization. Never emit allow, ask, or force_ask.

The engine probes this installed command with harmless sentinels and checks
content binding. Native SessionStart is not a pre-prompt readiness signal.
"""
import glob
import hashlib
import json
import math
import os
import re
import shlex
import signal
import subprocess
import sys
import socket
import tempfile
import threading

TOOL_NAME_MAP = {
    "run_command": "Bash",
    "write_file": "Write",
    "edit_file": "Edit",
    "read_file": "Read",
    "grep": "Grep",
    "glob": "Glob",
    "list_dir": "Glob",
}

KNOWN_TOOLS = {
    "run_command",
    "write_file",
    "edit_file",
    "read_file",
    "invoke_subagent",
    "define_subagent",
    "send_message",
    "grep",
    "glob",
    "list_dir",
}

EXPORTED_PATH_VARS = (
    "BOSS_BIN",
    "CUBE_BIN",
    "BOSS_BIN_DIR",
    "BOSS_STRUCTURED_OUTPUT",
    "HOME",
)

WRITE_PROGRAMS = {
    "touch",
    "rm",
    "mv",
    "cp",
    "tee",
    "chmod",
    "chown",
    "mkdir",
    "rmdir",
    "ln",
    "dd",
    "install",
    "truncate",
    "rsync",
}


def fail_closed(reason):
    sys.stderr.write("boss-agy-hook-adapter: DENY (fail-closed): %s\n" % (reason,))
    sys.stdout.write(json.dumps({"decision": "deny", "reason": reason}))
    sys.exit(0)


def sha256_file(path):
    with open(path, "rb") as handle:
        return "sha256:" + hashlib.sha256(handle.read()).hexdigest()


def load_json_file(path, what):
    try:
        with open(path, "r") as handle:
            data = json.load(handle)
    except Exception as exc:
        fail_closed("%s at %s is unreadable: %s" % (what, path, exc))
    if not isinstance(data, dict):
        fail_closed("%s at %s is not a JSON object" % (what, path))
    return data


def policy_path():
    path = os.environ.get("BOSS_AGY_POLICY")
    if not path:
        fail_closed("BOSS_AGY_POLICY is unset; refusing to authorize without engine-owned policy")
    return path


def load_policy():
    path = policy_path()
    policy = load_json_file(path, "authorization policy")
    run_id = policy.get("run_id")
    env_run = os.environ.get("BOSS_RUN_ID")
    if not isinstance(run_id, str) or not run_id:
        fail_closed("authorization policy is missing run_id")
    if env_run and env_run != run_id:
        fail_closed("BOSS_RUN_ID %r does not match policy run_id %r" % (env_run, run_id))
    if not env_run:
        fail_closed("BOSS_RUN_ID is unset; refusing to authorize without run identity")
    approval = policy.get("approval")
    if approval not in ("auto_approve", "deny_unless_prior_grant"):
        fail_closed("authorization policy has unknown approval %r" % (approval,))
    kinds = {"standard", "reviewer", "triage", "answer_agent", "review_guide", "progression"}
    if policy.get("worker_kind") not in kinds:
        fail_closed("authorization policy has unknown worker kind")
    for key in ("process_home", "workspace_path", "forwarder", "attestation_path"):
        if not isinstance(policy.get(key), str) or not os.path.isabs(policy[key]):
            fail_closed("authorization policy requires an absolute " + key)
    paths = policy.get("protected_paths")
    if not isinstance(paths, list) or not paths or any(not isinstance(p, str) or not os.path.isabs(p) for p in paths):
        fail_closed("authorization policy has malformed protected paths")
    scratch_root = policy.get("conversation_scratch_root")
    if scratch_root is not None and (not isinstance(scratch_root, str) or not os.path.isabs(scratch_root)):
        fail_closed("authorization policy has a malformed conversation scratch root")
    artifacts = policy.get("artifact_write_paths")
    if artifacts is None:
        artifacts = []
    if not isinstance(artifacts, list) or any(not isinstance(p, str) or not os.path.isabs(p) for p in artifacts):
        fail_closed("authorization policy has malformed artifact write paths")
    guards = policy.get("guards")
    if not isinstance(guards, list) or not guards:
        fail_closed("authorization policy requires a nonempty guard chain")
    for guard in guards:
        if not isinstance(guard, dict) or guard.get("matcher") not in ("*", ".*", "Bash") or not isinstance(guard.get("command"), str) or not guard["command"]:
            fail_closed("authorization policy has malformed guard configuration")
        timeout = guard.get("timeout_sec", 30)
        if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
            fail_closed("authorization policy has invalid guard timeout")
    return policy


def verify_binding(policy, require_self=True):
    attestation_path = policy.get("attestation_path")
    if not isinstance(attestation_path, str) or not attestation_path:
        fail_closed("authorization policy is missing attestation_path")
    attestation = load_json_file(attestation_path, "authorization attestation")
    required = policy.get("required_artifacts")
    if not isinstance(required, dict) or not all(key in required for key in ("adapter", "hooks", "policy", "settings", "forwarder")):
        fail_closed("authorization binding missing required artifact set")
    config = os.path.dirname(os.path.abspath(__file__))
    expected = {"adapter": os.path.abspath(__file__), "policy": os.path.join(config, "boss-agy-auth-policy.json"),
                "hooks": os.path.join(config, "hooks.json"),
                "settings": os.path.join(policy["process_home"], "../control/session-settings.json"),
                "forwarder": policy["forwarder"]}
    for name, path in expected.items():
        if os.path.abspath(required.get(name, "")) != os.path.abspath(path):
            fail_closed("authorization binding has unexpected run path for " + name)
    entries = attestation.get("entries")
    if not isinstance(entries, list) or not entries:
        fail_closed("authorization attestation has no entries")
    if len(entries) != len(required):
        fail_closed("authorization binding has incomplete artifact set")
    for name, path in required.items():
        if sum(entry.get("name") == name and entry.get("path") == path for entry in entries if isinstance(entry, dict)) != 1:
            fail_closed("authorization binding missing or misplaced " + name)
    for name in ("adapter", "forwarder"):
        if not os.access(required[name], os.X_OK):
            fail_closed("authorization binding: %s is not executable" % name)
    problems = []
    for entry in entries:
        if not isinstance(entry, dict):
            problems.append("malformed attestation entry")
            continue
        name = entry.get("name")
        path = entry.get("path")
        expected = entry.get("sha256")
        if not isinstance(path, str) or not isinstance(expected, str):
            problems.append("%s is missing path or sha256" % (name,))
            continue
        if not os.path.isfile(path):
            problems.append("%s missing: %s" % (name, path))
            continue
        actual = sha256_file(path)
        if actual != expected:
            problems.append("%s changed: %s" % (name, path))
        if require_self and name == "adapter":
            self_path = os.path.abspath(__file__)
            if os.path.abspath(path) != self_path and os.path.realpath(path) != os.path.realpath(self_path):
                problems.append("running adapter is not the attested adapter")
    if problems:
        fail_closed("authorization binding failed: " + "; ".join(problems))
    return attestation


def tool_name_of(payload):
    tool_call = payload.get("toolCall")
    if isinstance(tool_call, dict):
        name = tool_call.get("name")
        if isinstance(name, str) and name:
            return name
    for key in ("tool_name", "toolName"):
        name = payload.get(key)
        if isinstance(name, str) and name:
            return name
    return ""


def tool_args_of(payload):
    tool_call = payload.get("toolCall")
    if isinstance(tool_call, dict):
        args = tool_call.get("args")
        if not isinstance(args, dict):
            fail_closed("toolCall.args must be an object")
        return args
    for key in ("tool_input", "toolInput"):
        args = payload.get(key)
        if isinstance(args, dict):
            return args
    fail_closed("tool arguments are missing or not an object")


def first_str(args, keys):
    for key in keys:
        value = args.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def canonicalise(payload):
    native_name = tool_name_of(payload)
    args = tool_args_of(payload)
    aliases = (
        ("CommandLine", "command", "commandLine"),
        ("Cwd", "cwd", "workingDirectory"),
        ("Path", "path", "TargetFile", "targetFile", "target_file", "file_path", "AbsolutePath", "absolutePath"),
        ("Contents", "contents", "Content", "content"),
    )
    for keys in aliases:
        values = [args[key] for key in keys if key in args]
        if any(not isinstance(value, str) for value in values):
            fail_closed("native argument %s must be a string" % keys[0])
        if len(set(values)) > 1:
            fail_closed("conflicting native aliases for %s" % keys[0])
    if native_name == "run_command" and not first_str(args, aliases[0]):
        fail_closed("run_command requires a nonempty string command")
    if native_name in ("write_file", "read_file", "edit_file", "grep", "glob", "list_dir"):
        if not first_str(args, aliases[2]):
            fail_closed("%s requires a nonempty string path" % native_name)
    if native_name == "write_file" and not any(key in args for key in aliases[3]):
        fail_closed("write_file requires string content")
    canonical_name = TOOL_NAME_MAP.get(native_name, native_name)
    if native_name == "call_mcp_tool" or native_name.startswith("mcp_"):
        canonical_name = "mcp__agy_" + native_name
    tool_input = dict(args)
    command = first_str(args, ("CommandLine", "command", "commandLine"))
    if command is not None:
        tool_input["command"] = command
    cwd = first_str(args, ("Cwd", "cwd", "workingDirectory"))
    if cwd is not None:
        tool_input["cwd"] = cwd
    path = first_str(
        args,
        ("Path", "path", "TargetFile", "targetFile", "target_file", "file_path", "AbsolutePath", "absolutePath"),
    )
    if path is not None:
        tool_input["file_path"] = path
        if native_name in ("grep", "glob", "list_dir"):
            tool_input["path"] = path
    content = first_str(args, ("Contents", "contents", "Content", "content"))
    if content is not None:
        tool_input["content"] = content
    out = {
        "hook_event_name": "PreToolUse",
        "tool_name": canonical_name,
        "tool_input": tool_input,
        "agy_tool_name": native_name,
    }
    conversation = payload.get("conversationId") or payload.get("session_id") or payload.get("sessionId")
    if isinstance(conversation, str):
        out["session_id"] = conversation
    tool_call = payload.get("toolCall")
    if isinstance(tool_call, dict) and isinstance(tool_call.get("id"), str):
        out["tool_use_id"] = tool_call["id"]
    cwd_payload = payload.get("cwd")
    if cwd is not None:
        out["cwd"] = cwd
    elif isinstance(cwd_payload, str):
        out["cwd"] = cwd_payload
    return native_name, args, out


def emit(decision, reason=None, overwrite=None):
    body = {"decision": decision}
    if reason is not None:
        body["reason"] = reason
    if overwrite:
        body["overwrite"] = overwrite
    sys.stdout.write(json.dumps(body))
    sys.exit(0)


def authorized(policy, overwrite=None):
    emit(policy["approval"], overwrite=overwrite)


# REVIEWER_BLOCKED_FRAGMENT
# COMMAND_TOKENIZER_FRAGMENT


# Variables whose value bash later executes or evaluates (command hash tables,
# startup files, xtrace prompts, callbacks, option strings). They are denied by
# name wherever they could be written, not by enumerating spellings.
DYNAMIC_NAMES = "BASH_ALIASES|BASH_CMDS|BASH_ENV|PS4|PS0|PROMPT_COMMAND|SHELLOPTS|BASHOPTS"
DYNAMIC_NAME_RE = re.compile(r"(?<![A-Za-z0-9_])(?:%s)(?![A-Za-z0-9_])" % DYNAMIC_NAMES)
DYNAMIC_ASSIGN_RE = re.compile(r"^(?:%s)(?:\[|\+?=)" % DYNAMIC_NAMES)
LITERAL_NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
NAME_BINDERS = {"export", "declare", "typeset", "local", "readonly", "unset", "read", "getopts", "mapfile", "readarray"}
# Contexts that evaluate their text arithmetically, including a `$(...)`
# subscript: `(( ))`, `$[ ]`, `[[ ]]`, `let`, `${v:expr}` / `${a[expr]}`,
# `name[expr]` subscripts and integer-attribute declarations.
ARITHMETIC_EVAL_RE = re.compile(
    r"\(\(|\$\[|\[\[|(?<![A-Za-z0-9_])let(?![A-Za-z0-9_])|\$\{[^}]*[:\[]"
    r"|\b(?:declare|typeset|local)\b[^;|&\n]*\s-[A-Za-z]*i|[A-Za-z_][A-Za-z0-9_]*\["
)


def text_outside_single_quotes(command):
    # Single-quoted content is data; blank it. ANSI-C content is kept both raw
    # and escape-decoded because bash decodes it before use.
    out, quote, escaped, skip, ansi = [], None, False, False, []
    for index, char in enumerate(command):
        if skip:
            skip = False
            continue
        if quote == ANSI:
            if escaped:
                escaped = False
                ansi.append(char)
            elif char == chr(92):
                escaped = True
                ansi.append(char)
            elif char == chr(39):
                quote = None
                raw = "".join(ansi)
                ansi = []
                try:
                    decoded = raw.encode("latin-1", "backslashreplace").decode("unicode_escape")
                except (UnicodeError, ValueError):
                    fail_closed("ANSI-C quoted text cannot be decoded for inspection")
                out.append(raw + " " + decoded)
            else:
                ansi.append(char)
            continue
        if quote == chr(39):
            if char == chr(39):
                quote = None
                out.append(char)
            else:
                out.append(" ")
            continue
        if escaped:
            escaped = False
            out.append(char)
            continue
        if char == chr(92):
            escaped = True
            out.append(char)
            continue
        if quote is None and is_ansi_open(command, index, quote):
            quote, skip = ANSI, True
            continue
        if char in (chr(34), chr(39)):
            if quote == char:
                quote = None
            elif quote is None:
                quote = char
        out.append(char)
    if quote == ANSI:
        fail_closed("unterminated ANSI-C quote cannot be inspected")
    return "".join(out)


def neutralize_expanding_heredoc_quotes(command):
    # Quote characters inside an unquoted (expanding) heredoc are literal data.
    # Replace them with spaces so quote tracking of later shell code cannot be
    # inverted by an apostrophe in the body, while `$`, `[` and `(` stay visible
    # for the arithmetic / substitution scan.
    command = strip_line_continuations(command)
    masked = list(command)
    for group in command_records(command, True):
        if not group.body_span or not group.expanding_body:
            continue
        start, end = group.body_span
        for index in range(start, end):
            if masked[index] in ("'", '"'):
                masked[index] = " "
    return "".join(masked)


def reject_dynamic_text(command):
    # Inspect shell code only. The body of a non-expanding (quoted-delimiter)
    # heredoc is data, so it is masked first. Quotes inside an expanding
    # heredoc are literal, so they are spaced out before
    # `text_outside_single_quotes` blanks single-quoted data. Neither can be
    # evaluated by the shell.
    code = neutralize_expanding_heredoc_quotes(mask_literal_heredoc_bodies(command))
    text = text_outside_single_quotes(code)
    if DYNAMIC_NAME_RE.search(text):
        fail_closed("variables that bash later executes or evaluates cannot be named")
    # A substitution inside single-quoted or heredoc data is inert until an
    # arithmetic context, integer attribute or subscript in the shell code
    # evaluates it. Data without a substitution, or shell code without such an
    # evaluating context, needs no further inspection.
    if ("$(" in command or chr(96) in command) and ARITHMETIC_EVAL_RE.search(text):
        fail_closed("arithmetic evaluation of text containing a substitution cannot be inspected")
    if "$(" in command or chr(96) in command:
        # A literal substitution is data until bash evaluates it. Evaluation
        # is reachable through indirect/prompt expansion, `test -v`, and
        # evaluator builtins whose name may be quoted, so deny every
        # non-plain `${...}` and every evaluator after quote removal.
        if re.search(r"\$\{(?![A-Za-z_][A-Za-z0-9_]*\})", text):
            fail_closed("non-plain parameter expansion beside a literal substitution cannot be inspected")
        for group in command_groups(command):
            rest = strip_prefixes(group)
            if not rest:
                continue
            program = os.path.basename(rest[0])
            flags = [a for a in rest[1:] if a.startswith("-") and not a.startswith("--")]
            if program in ("let", "test", "[", "[[", "((", "eval"):
                fail_closed("evaluator beside a literal substitution cannot be inspected")
            if program in ("declare", "typeset", "local") and any("i" in a for a in flags):
                fail_closed("integer attribute beside a literal substitution cannot be inspected")


def reject_dynamic_assignment_word(token):
    if token.startswith("-") or "=" not in token:
        return
    name = token.split("=", 1)[0]
    if not LITERAL_NAME_RE.fullmatch(name) and re.search(r"[$`*?\[{~]", name):
        fail_closed("assignment word with a non-literal name cannot be inspected")


def reject_dynamic_bindings(group):
    tokens = without_redirections(group)
    for token in tokens:
        if DYNAMIC_ASSIGN_RE.match(token):
            fail_closed("dynamic shell command assignment is denied")
    after_env = False
    for token in tokens:
        if after_env:
            reject_dynamic_assignment_word(token)
        if os.path.basename(token) == "env":
            after_env = True
    rest = strip_prefixes(group)
    if not rest:
        return
    reject_dynamic_assignment_word(rest[0])
    program = os.path.basename(rest[0])
    args = rest[1:]
    if program == "hash" and any(a.startswith("-") and not a.startswith("--") and "p" in a for a in args):
        fail_closed("hash -p binds a command name to a path and cannot be inspected")
    if program in ("declare", "typeset", "local") and any(
            a.startswith("-") and not a.startswith("--") and "n" in a for a in args):
        fail_closed("namerefs cannot be inspected")
    if program == "set" and any(
            "xtrace" in a or (a.startswith("-") and not a.startswith("--") and "x" in a) for a in args):
        fail_closed("xtrace evaluates PS4 and is denied")
    targets = None
    if program in NAME_BINDERS:
        targets = [a for a in args if not a.startswith("-")]
    elif program == "printf":
        targets = []
        if args and re.search(r"[$`*?\[{~]", args[0]):
            fail_closed("printf option or format must be statically known")
        for index, a in enumerate(args):
            if a == "-v" and index + 1 < len(args):
                targets.append(args[index + 1])
            elif a.startswith("-v") and len(a) > 2:
                targets.append(a[2:])
    if targets is None:
        return
    for target in targets:
        name = target.split("=", 1)[0]
        if DYNAMIC_NAME_RE.search(target):
            fail_closed("binding a variable that bash later executes is denied")
        if not LITERAL_NAME_RE.fullmatch(name):
            fail_closed("binding a variable through a non-literal name is denied")


def nested_shell_bodies(group):
    reject_dynamic_bindings(group)
    normalized = without_redirections(group)
    for index, token in enumerate(normalized):
        if os.path.basename(token) == "env" and any(
                option.startswith("--split-string") or
                (option.startswith("-") and not option.startswith("--") and "S" in option)
                for option in normalized[index + 1:]):
            fail_closed("env split-string execution cannot be inspected")
    rest = strip_prefixes(group)
    if rest and os.path.basename(rest[0]) in ("jobs", "shopt"):
        fail_closed("shell execution and option builtins cannot be inspected")
    if rest and any(c in rest[0] for c in "<>"):
        fail_closed("unsupported redirection attached to an executable")
    base = os.path.basename(rest[0]) if rest else ""
    if base in {"if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "case", "esac", "{", "}", "!", "eval", "source", ".", "trap", "alias", "compgen", "coproc", "mapfile", "readarray"}:
        fail_closed("unsupported shell control or dynamic execution")
    if rest and os.path.basename(rest[0]) in ("sh", "bash", "zsh", "dash", "ksh"):
        for index, arg in enumerate(rest[1:], 1):
            if re.fullmatch(r"-[A-Za-z]*c[A-Za-z]*", arg) and index + 1 < len(rest):
                return [rest[index + 1]]
        fail_closed("shell stdin or script execution cannot be inspected")
    # A Python `-c` string or stdin body is Python source, not shell. It is not
    # shell-parsed (its quotes, backticks and `$` mean nothing to bash). Python
    # is as opaque here as a script file the worker runs by name; the shell
    # text that carries it is still fully inspected. Protected-root spellings
    # inside those bodies are scanned separately (see python_inline_bodies).
    if base == "find":
        bodies = []
        for index, arg in enumerate(rest):
            if arg in ("-exec", "-execdir", "-ok", "-okdir"):
                end = next((i for i in range(index + 1, len(rest)) if rest[i] in (";", "+")), len(rest))
                bodies.append(shlex.join(rest[index + 1:end]))
        return bodies
    return []


PYTHON_PROGRAMS = {"python", "python2", "python3", "ipython"}


def is_python_program(name):
    return name in PYTHON_PROGRAMS or re.fullmatch(r"python[0-9.]+", name)


def python_inline_bodies(group):
    rest = strip_prefixes(group)
    # Launchers such as uv/poetry/pipx can precede the interpreter.
    start = next((i for i, arg in enumerate(rest)
                  if is_python_program(os.path.basename(arg))), None)
    if start is None:
        return []
    bodies = []
    arguments = iter(rest[start + 1:])
    for arg in arguments:
        if arg == "--" or arg == "-" or not arg.startswith("-"):
            break
        if arg == "--check-hash-based-pycs":
            next(arguments, "")
            continue
        if arg.startswith("--"):
            continue
        # c/m end option processing; W/X consume an attached or next value.
        for offset, flag in enumerate(arg[1:], 2):
            if flag in "cmWX":
                value = arg[offset:] or next(arguments, "")
                if flag == "c":
                    bodies.append(value)
                break
        else:
            continue
        if flag in "cm":
            break
    body = getattr(group, "body", "") or ""
    if body:
        bodies.append(body)
    here = here_string_body(group)
    if here:
        bodies.append(here)
    return bodies


def allowed_scratch_spellings(policy):
    spellings = []
    bases = []
    base = conversation_scratch_dir(policy)
    if base:
        bases.append(base)
    root = policy.get("conversation_scratch_root")
    conv = policy.get("conversation_id")
    if isinstance(root, str) and isinstance(conv, str) and conv:
        bases.append(os.path.join(root, conv))
        bases.append(os.path.join(os.path.realpath(root), conv))
    suffixes = (
        "scratch",
        os.path.join(".system_generated", "steps"),
        os.path.join(".system_generated", "tasks"),
    )
    process_home = policy.get("process_home") or ""
    for home in bases:
        for suffix in suffixes:
            path = os.path.join(home, suffix)
            spellings.extend(protected_spellings(path))
            if process_home and path.startswith(process_home + os.sep):
                rel = path[len(process_home):]
                spellings.append("~" + rel)
                spellings.append("$HOME" + rel)
                spellings.append("${HOME}" + rel)
    unique = []
    for spelling in sorted(set(spellings), key=len, reverse=True):
        if spelling and spelling not in unique:
            unique.append(spelling)
    return unique


def python_body_touches_protected(body, policy):
    # Literal scan only: the body is Python, so `$()` is not shell. Mask this
    # run's allowed scratch/steps/tasks spellings first so a body that opens
    # `{scratch}/lib.rs` stays allowed.
    text = body
    for spelling in allowed_scratch_spellings(policy):
        for match in re.finditer(re.escape(spelling) + r"[^\s\"']*", text):
            suffix = match.group()[len(spelling):]
            if ".." in suffix.replace(chr(92), "/").split("/"):
                return True
            if suffix and not suffix.startswith("/"):
                return True
        text = text.replace(spelling, "ALLOWED_SCRATCH")
    needles = []
    for root in policy.get("protected_paths", []):
        needles.extend(protected_spellings(root))
    process_home = policy.get("process_home") or ""
    if process_home:
        gemini = os.path.join(process_home, ".gemini")
        needles.extend(protected_spellings(gemini))
        needles.extend(["~/.gemini", "$HOME/.gemini", "${HOME}/.gemini"])
    for needle in needles:
        if needle and needle in text:
            return True
    conv = policy.get("conversation_id")
    for match in re.finditer(r"(?:antigravity-cli/)?brain[/\\]([A-Za-z0-9][A-Za-z0-9_-]{0,127})", text):
        if not isinstance(conv, str) or match.group(1) != conv:
            return True
    return False


def mask_heredoc_bodies(command, expanding):
    # Blank heredoc bodies. With `expanding` False only the bodies of heredocs
    # whose delimiter is quoted are blanked (bash performs no expansion in
    # them, so they are data); with True, the bodies bash does expand are
    # blanked as well. Returns the text and the expanding bodies it blanked.
    command = strip_line_continuations(command)
    masked = list(command)
    expanded = []
    for group in command_records(command, True):
        if not group.body_span or (group.expanding_body and not expanding):
            continue
        start, end = group.body_span
        if group.expanding_body:
            expanded.append(command[start:end])
        masked[start:end] = ' ' * (end - start)
    return ''.join(masked), expanded


def mask_literal_heredoc_bodies(command):
    return mask_heredoc_bodies(command, False)[0]


def expanding_body_substitutes(body):
    # Quote characters mean nothing inside an unquoted heredoc: only a
    # backslash escape protects a `$(` or backtick from bash.
    escaped = False
    for index, char in enumerate(body):
        if escaped:
            escaped = False
        elif char == chr(92):
            escaped = True
        elif char == chr(96) or body[index:index + 2] == "$(":
            return True
    return False


def has_substitution(command):
    masked, expanded = mask_heredoc_bodies(command, True)
    if any(expanding_body_substitutes(body) for body in expanded):
        return True
    quote, escaped, skip = None, False, False
    for index, char in enumerate(masked):
        if skip:
            skip = False
            continue
        if escaped:
            escaped = False
            continue
        if char == chr(92) and quote != chr(39):
            escaped = True
            continue
        if is_ansi_open(masked, index, quote):
            quote, skip = ANSI, True
            continue
        if quote == ANSI:
            if char == chr(39):
                quote = None
            continue
        if char in (chr(34), chr(39)):
            if quote == char:
                quote = None
            elif quote is None:
                quote = char
        if quote != chr(39) and (char == chr(96) or masked[index:index + 2] == "$("):
            return True
    return False


def inspected_commands(command, depth=0):
    if depth > 4:
        fail_closed("shell nesting exceeds the inspected depth")
    if any(group.error for group in command_records(command)):
        fail_closed("incomplete shell syntax cannot be inspected")
    if has_substitution(command):
        fail_closed("command substitution is structurally denied")
    reject_dynamic_text(command)
    yield command
    for group in command_groups(command):
        for body in nested_shell_bodies(group):
            yield from inspected_commands(body, depth + 1)


def iter_groups(command):
    for body in inspected_commands(command):
        yield from command_groups(body)


def basename_of(group):
    rest = strip_prefixes(group)
    if not rest:
        return ""
    return os.path.basename(rest[0])


def has_redirect(command):
    lexer = shlex.shlex(command, posix=False, punctuation_chars="><|")
    lexer.whitespace_split = True
    tokens = list(lexer)
    for index, token in enumerate(tokens):
        if token in (">", ">>", ">|", "<>"):
            target = tokens[index + 1] if index + 1 < len(tokens) else ""
            if target.strip(chr(34) + chr(39)) != "/dev/null" and not re.fullmatch(r"&(?:[0-9]+|-)", target):
                return True
    return False


# Resolve the exact environment inherited from the worker at spawn.
# Commands cannot rebind these variables and use the trusted resolution.
# Every name expand_exported substitutes is rebound-protected: assignments,
# binding builtins, printf -v, env/exec unbinding, coproc/wait -p, subscripted
# assignments, non-plain ${...}, and arithmetic / `[[ ]]` in a command that
# also expands one.
TRUSTED_PATH_VARS = EXPORTED_PATH_VARS
TRUSTED_NAMES = "|".join(TRUSTED_PATH_VARS)
TRUSTED_VAR_RE = re.compile(r"\$(?:(%s)\b|\{(%s)\})" % (TRUSTED_NAMES, TRUSTED_NAMES))
TRUSTED_VAR_ASSIGN_RE = re.compile(r"^(?:%s)(?:\+?=|\[)" % TRUSTED_NAMES)
TRUSTED_NAME_IN_TOKEN_RE = re.compile(r"(?<![A-Za-z0-9_])(?:%s)(?![A-Za-z0-9_])" % TRUSTED_NAMES)
# Builtins that bind or unbind variables by name. Their name arguments can be
# spelled indirectly (brace expansion, namerefs, "$name"), so with a trusted
# variable in play they are denied whatever their arguments look like.
BINDING_BUILTINS = {"export", "declare", "typeset", "local", "readonly", "unset", "read", "getopts", "mapfile", "readarray", "let"}


ANY_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(?:\[[^\]]*\])?\+?=")
SUBSCRIPT_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*\[")
PLAIN_BRACE_EXPANSION_RE = re.compile(r"\$\{[A-Za-z_][A-Za-z0-9_]*\}")


# Contexts bash evaluates arithmetically after expansion: `(( ))`, `$[ ]`,
# and `[[ ]]`. Non-plain `${...}` (subscripts, substring offsets, `${!` / `${#`)
# is a separate fail-closed rule: enumerating those forms misses assignments.
ARITHMETIC_CONTEXT_RES = (
    re.compile(r"\(\((.*?)(?:\)\)|\Z)", re.S),
    re.compile(r"\$\[(.*?)(?:\]|\Z)", re.S),
    re.compile(r"\[\[(.*?)(?:\]\]|\Z)", re.S),
)


def arithmetic_context_rebinds(command):
    masked = TRUSTED_VAR_RE.sub("", command)
    for pattern in ARITHMETIC_CONTEXT_RES:
        for match in pattern.finditer(masked):
            body = match.group(1)
            if "$" in body or TRUSTED_NAME_IN_TOKEN_RE.search(body):
                return True
    return False


def non_plain_parameter_expansion(command):
    return "${" in PLAIN_BRACE_EXPANSION_RE.sub("", command)


def prefix_clears_environment(group):
    normalized = without_redirections(group)
    rest = strip_prefixes(group)
    prefix = normalized[:len(normalized) - len(rest)] if rest else list(normalized)
    # Unknown wrapper options may alter the environment or working directory.
    # Only option-free env/exec prefixes are safe for inherited substitution.
    for index, token in enumerate(prefix):
        if os.path.basename(token) in ("env", "exec"):
            if index + 1 < len(prefix) and prefix[index + 1].startswith("-") and prefix[index + 1] != "--":
                return True
    return False


def wait_binds_pid_variable(group):
    rest = strip_prefixes(group)
    if not rest or os.path.basename(rest[0]) != "wait":
        return False
    for token in rest[1:]:
        if token in ("-p", "--pid") or token.startswith("--pid="):
            return True
        if token.startswith("-") and not token.startswith("--") and "p" in token[1:]:
            return True
    return False


def reject_trusted_rebinding(command, groups):
    # Bash joins backslash-newline before parsing, so test the joined text.
    command = strip_line_continuations(command)
    uses_trusted = TRUSTED_VAR_RE.search(command) is not None
    # In a command that expands a trusted name, an arithmetic context that
    # names a trusted variable or contains any other expansion (which could
    # compose the name) may assign it; expand_exported would then substitute
    # the stale inherited value. Names outside such contexts, including
    # quoted message text and search patterns, are plain data.
    if uses_trusted and arithmetic_context_rebinds(command):
        fail_closed("an arithmetic context in a command using an engine-provided path variable could reassign it")
    if uses_trusted and non_plain_parameter_expansion(command):
        fail_closed("a parameter expansion in a command using an engine-provided path variable could reassign it")
    for group in groups:
        if uses_trusted and prefix_clears_environment(group):
            fail_closed("clearing environment in a command using an engine-provided path variable is denied")
        if uses_trusted and wait_binds_pid_variable(group):
            fail_closed("wait -p in a command using an engine-provided path variable is denied")
        normalized = without_redirections(group)
        rest = list(strip_prefixes(group))
        # Prefix and standalone assignment positions bind a variable; the same
        # words as ordinary arguments (`rg BOSS_BIN`) are plain data.
        assigned = list(normalized[:len(normalized) - len(rest)] if rest else normalized)
        # `NAME+=v` and `NAME[i]=v` are assignments too, though strip_prefixes
        # only recognizes `NAME=`.
        while rest and (ANY_ASSIGN_RE.match(rest[0]) or SUBSCRIPT_ASSIGN_RE.match(rest[0])):
            assigned.append(rest.pop(0))
        for token in assigned:
            if TRUSTED_VAR_ASSIGN_RE.match(token):
                fail_closed("reassigning an engine-provided path variable is denied")
            if uses_trusted and SUBSCRIPT_ASSIGN_RE.match(token):
                fail_closed("subscripted assignment in a command using an engine-provided path variable is denied")
        if not rest:
            continue
        program = os.path.basename(rest[0])
        binds = program in BINDING_BUILTINS or (program == "printf" and "-v" in rest[1:])
        if binds and (uses_trusted or any(TRUSTED_NAME_IN_TOKEN_RE.search(token) for token in rest[1:])):
            fail_closed("rebinding variables is denied when an engine-provided path variable is in use")


def exported_paths():
    return {name: value for name in EXPORTED_PATH_VARS
            if (value := os.environ.get(name)) and os.path.isabs(value)
            and not any(c in value for c in "\n\0")}


def expand_exported(path, literal=None, segments=None, shell_token=False):
    values = exported_paths()
    if not shell_token:
        def replacer(match):
            name = match.group(1) or match.group(2)
            if name in values:
                return values[name]
            return match.group(0)

        return re.sub(r"\$(?:([A-Za-z_][A-Za-z0-9_]*)|\{([A-Za-z_][A-Za-z0-9_]*)\})", replacer, path)
    if segments is None and "$" in path:
        fail_closed("unbraced path variable expansion cannot be verified")
    out = []
    index = 0
    length = len(path)
    while index < length:
        if path[index] != "$" or (literal and index < len(literal) and literal[index] is True):
            out.append(path[index])
            index += 1
            continue
        if index + 1 < length and path[index + 1] == "{":
            close = path.find("}", index + 2)
            if close != -1:
                name = path[index + 2:close]
                if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", name) and name in values:
                    if (segments is None or close >= len(segments)
                            or any(segment != segments[index] for segment in segments[index:close + 1])
                            or any(quoted is True for quoted in (literal or [])[index:close + 1])):
                        fail_closed("braced path variable expansion crosses a quote or escape boundary")
                    out.append(values[name])
                    index = close + 1
                    continue
            out.append("$")
            index += 1
            continue
        if segments is None or index >= len(segments):
            fail_closed("unbraced path variable expansion cannot be verified")
        seg = segments[index]
        cursor = index + 1
        if cursor < length and cursor < len(segments) and segments[cursor] == seg and (path[cursor].isalpha() or path[cursor] == "_"):
            cursor += 1
            while cursor < length and cursor < len(segments) and segments[cursor] == seg and (path[cursor].isalnum() or path[cursor] == "_"):
                cursor += 1
        name = path[index + 1:cursor]
        if name in values:
            out.append(values[name])
            index = cursor
            continue
        out.append("$")
        index += 1
    return "".join(out)


def resolve_path(path, policy, cwd=None, pre_expanded=False):
    if pre_expanded:
        expanded = path
    else:
        expanded = expand_exported(path)
        if re.match(r"^~[^/]", expanded):
            fail_closed("unresolved shell path expansion")
        if "$" in expanded:
            fail_closed("unresolved shell path expansion")
        if expanded == "~" or expanded.startswith("~/"):
            expanded = policy["process_home"] + expanded[1:]
    if not os.path.isabs(expanded):
        expanded = os.path.join(cwd or policy["workspace_path"], expanded)
    return os.path.realpath(expanded)


def protected_spellings(root):
    spellings = [os.path.normpath(root), os.path.realpath(root), root]
    unique = []
    for spelling in spellings:
        if spelling not in unique:
            unique.append(spelling)
    return unique


# CONVERSATION_SCRATCH_FRAGMENT


def conversation_scratch_dir(policy):
    return conversation_scratch_base(policy.get("conversation_scratch_root"), policy.get("conversation_id"))


def within_conversation_scratch(resolved, policy):
    return conversation_scratch_contains(resolved, conversation_scratch_dir(policy))


def path_is_protected(path, policy, cwd=None, pre_expanded=False):
    expanded = resolve_path(path, policy, cwd, pre_expanded=pre_expanded)
    if within_conversation_scratch(expanded, policy):
        return False
    for root in policy.get("protected_paths", []):
        for spelling in protected_spellings(root):
            root_abs = spelling.rstrip(os.sep)
            if expanded == root_abs or expanded.startswith(root_abs + os.sep):
                return True
    return False


def glob_prefix_spellings(static):
    prefixes = [static, os.path.normpath(static), os.path.realpath(static)]
    existing = static
    while not os.path.exists(existing) and os.path.dirname(existing) != existing:
        existing = os.path.dirname(existing)
    prefixes.append(os.path.realpath(existing))
    normalized = os.path.normpath(static)
    if normalized != static:
        existing = normalized
        while not os.path.exists(existing) and os.path.dirname(existing) != existing:
            existing = os.path.dirname(existing)
        prefixes.append(os.path.realpath(existing))
        prefixes.append(os.path.realpath(normalized))
    return prefixes


def glob_wildcard_uses_unmodeled_brackets(wildcard):
    # Python's glob/fnmatch does not implement POSIX classes, equivalence
    # classes, collating symbols or bash's `[^` negation. A "no matches" result is
    # not proof the pattern stays inside scratch.
    return any(marker in wildcard for marker in ("[[", "[!", "[^"))


def glob_touches_protected(expanded, policy):
    # Keep the un-normalized static prefix so a symlink-then-`..` glob is
    # compared after physical resolution, matching bash, not after lexically
    # collapsing `link/../` away from the protected root.
    static = re.split(r"[\*\?\[]", expanded, 1)[0].rstrip(os.sep) or os.sep
    prefixes = glob_prefix_spellings(static)
    # A glob whose fixed prefix is inside this run's conversation scratch
    # directory is that directory's own business, provided its wildcard part
    # cannot climb out (`..` after a wildcard can step through a link) or reach
    # the engine-read transcript (a wildcard directly under the conversation
    # directory or its `.system_generated` directory could match `logs`).
    base = conversation_scratch_dir(policy)
    wildcard = expanded[len(static):]
    if (
        base is not None
        and ".." not in wildcard.split(os.sep)
        and not glob_wildcard_uses_unmodeled_brackets(wildcard)
    ):
        inner = [os.path.realpath(prefix) for prefix in prefixes]
        hubs = (base, os.path.join(base, ".system_generated"))
        if all(within_conversation_scratch(p, policy) and p not in hubs for p in inner):
            # A wildcard can match a link that leads out of the directory, so
            # every actual match must resolve inside it too.
            if all(within_conversation_scratch(os.path.realpath(match), policy) for match in glob.glob(expanded)):
                return False
    for root in policy.get("protected_paths", []):
        for spelling in protected_spellings(root):
            root_abs = spelling.rstrip(os.sep)
            if any(root_abs.startswith(prefix) or prefix.startswith(root_abs + os.sep) or prefix == root_abs for prefix in prefixes):
                return True
    return False


PATH_TEXT_PROGRAMS = {'echo', 'printf'}
NON_PATH_PROGRAMS = {'printenv', 'test', '['}
GREP_FAMILY = {'grep', 'rg', 'sed', 'awk', 'gawk'}
TEXT_FLAGS = {
    'grep': ('-e', '--regexp'),
    'rg': ('-e', '--regexp'),
    'sed': ('-e', '--expression'),
    'awk': ('-e', '--source'),
    'gawk': ('-e', '--source'),
}
FILE_FLAGS = {
    'grep': ('-f', '--file'),
    'rg': ('-f', '--file'),
    'sed': ('-f', '--file'),
    'awk': ('-f', '--file'),
    'gawk': ('-f', '--file'),
}
GREP_VALUE_FLAGS = frozenset({'-g', '--glob', '--iglob', '-t', '--type', '-A', '-B', '-C', '-m', '--max-depth'})
STDIN_ARGV_RUNNERS = {'xargs', 'parallel'}
STDIN_ARGV_SAFE = {'echo', 'printf', 'true', 'false', ':', 'printenv'}


def operand_path_texts(token, role, program):
    # Candidate path texts for one classified operand, with the start offset
    # in `token` so quote-segment provenance stays aligned. Positional operands
    # are kept verbatim (a name such as `-falias` or `name=alias` is the file
    # the program opens); only syntactic wrappers are removed for other roles.
    def item(text, start=0):
        return (text, start)

    if role == 'literal':
        return [item(token)]
    if role == 'operand':
        candidates = [item(token)]
        if ASSIGNMENT_RE.match(token):
            # dd-style `if=path` operands also name a path in their value.
            start = token.find('=') + 1
            candidates.append(item(token[start:], start))
        return candidates
    if role == 'assignment' or role == 'option':
        if '=' in token:
            start = token.find('=') + 1
            return [item(token[start:], start)]
        return [item(token)]
    if role == 'redirect_attached':
        stripped = re.sub(r'^(?:[0-9]+|&)?[<>|]+', '', token)
        return [item(stripped, len(token) - len(stripped))]
    if role == 'dynamic_short':
        flags = FILE_FLAGS.get(program, ()) + TEXT_FLAGS.get(program, ())
        candidates = [item(token)]
        for i, char in enumerate(token[1:], 1):
            if '-' + char in flags and token[i + 1:]:
                candidates.append(item(token[i + 1:], i + 1))
                break
        return candidates
    if role == 'attached':
        flags = FILE_FLAGS.get(program, ())
        for i, char in enumerate(token[1:], 1):
            if '-' + char in flags:
                suffix = token[i + 1:]
                return [item(suffix or token, (i + 1) if suffix else 0)]
    return [item(token)]


def stdin_argv_program(group):
    normalized = without_redirections(group)
    rest = strip_prefixes(group)
    prefixes = normalized[:len(normalized) - len(rest)] if rest else normalized
    if any(os.path.basename(token) in STDIN_ARGV_RUNNERS for token in prefixes if not ASSIGNMENT_RE.match(token)):
        return os.path.basename(rest[0]) if rest else 'echo'
    if rest and os.path.basename(rest[0]) in STDIN_ARGV_RUNNERS:
        inner, index = rest[1:], 0
        while index < len(inner) and inner[index].startswith('-'):
            option = inner[index]
            index += 1
            if option == '--':
                break
        return os.path.basename(inner[index]) if index < len(inner) else 'echo'
    return None


MULTI_WORD_EXPANSION_RE = re.compile(r"^\$(?:@|\{(?:@[^}]*|[^}]*\[@\][^}]*|![^}]*@)\})")


def has_brace_expansion(token, literal, start):
    depth, comma = 0, False
    for j in range(start, len(token)):
        if j < len(literal) and literal[j]:
            continue
        char = token[j]
        if char == '{':
            depth += 1
        elif char == '}':
            depth -= 1
            if depth == 0:
                return comma or '..' in token[start:j]
        elif char == ',' and depth == 1:
            comma = True
    return False


def has_unquoted_glob_or_expansion(token, literal):
    for i, char in enumerate(token):
        quoted = literal[i] if i < len(literal) else False
        if quoted == 'd':
            # "$@" and "${a[@]}" still expand to one word per element.
            if char == '$' and MULTI_WORD_EXPANSION_RE.match(token[i:]):
                return True
            continue
        if quoted:
            continue
        if char == '$' or char in '*?[':
            return True
        if char == '{' and has_brace_expansion(token, literal, i):
            return True
    return False


def token_literal(group, index):
    return group.literal_chars[index] if index < len(group.literal_chars) else []


def token_segments(group, index):
    segs = getattr(group, "quote_segments", None)
    if segs is None or index >= len(segs):
        return None
    return segs[index]


def path_operand_positions(group):
    # Default operands to paths; exclude known text and script positions.
    # Returns {token index: syntactic role}.
    rest_ids = set(strip_prefix_indices(group))
    rest = strip_prefixes(group)
    program = os.path.basename(rest[0]) if rest else ''
    text_flags = TEXT_FLAGS.get(program, ())
    file_flags = FILE_FLAGS.get(program, ())
    operand, skip_next, skip_as_path, pattern_seen, end_of_options = 0, False, False, False, False
    positions = {}
    value_flags = GREP_VALUE_FLAGS if program in ('grep', 'rg') else frozenset()

    def keep_redirect(index):
        return str(positions.get(index, '')).startswith('redirect')

    def mark_text_slot(index, token, role='operand'):
        if has_unquoted_glob_or_expansion(token, token_literal(group, index)):
            positions[index] = role

    for index, token in enumerate(group):
        redirect = re.match(r'^(?:[0-9]+|&)?(?:>\||>>?|<>|<<-?|<)(.*)$', token)
        if redirect:
            if token.startswith('<<'):
                skip_next, skip_as_path = not redirect.group(1), False
            elif redirect.group(1):
                positions[index] = 'redirect_attached'
            elif index + 1 < len(group):
                positions[index + 1] = 'redirect_target'
            continue
        if skip_next:
            if not keep_redirect(index):
                if skip_as_path or has_unquoted_glob_or_expansion(token, token_literal(group, index)):
                    positions[index] = 'operand'
            skip_next, skip_as_path = False, False
            continue
        if keep_redirect(index):
            continue
        if index not in rest_ids:
            if not token.startswith('-'):
                positions.setdefault(index, 'assignment' if ASSIGNMENT_RE.match(token) else 'operand')
            continue
        if operand == 0:
            positions[index] = 'operand'
            operand += 1
            continue
        if not end_of_options and token == '--':
            end_of_options = True
            operand += 1
            continue
        if end_of_options:
            positions[index] = 'literal'
            operand += 1
            continue
        if token.startswith('-'):
            if token.startswith('--'):
                name, eq, _value = token.partition('=')
                if name in text_flags:
                    pattern_seen = True
                    if eq:
                        mark_text_slot(index, token, 'option')
                    else:
                        skip_next, skip_as_path = True, False
                elif name in file_flags:
                    pattern_seen = True
                    if eq:
                        positions[index] = 'option'
                    else:
                        skip_next, skip_as_path = True, True
                elif has_unquoted_glob_or_expansion(token, token_literal(group, index)):
                    positions[index] = 'operand'
                elif eq:
                    positions[index] = 'option'
                elif name in value_flags:
                    skip_next, skip_as_path = True, False
            else:
                if has_unquoted_glob_or_expansion(token, token_literal(group, index)):
                    # Keep the whole word, and also check any value a
                    # recognized file or text flag carries attached.
                    positions[index] = 'dynamic_short'
                else:
                    consumed = False
                    for i, char in enumerate(token[1:], 1):
                        flag = '-' + char
                        if flag in text_flags or flag in file_flags:
                            attached = token[i + 1:]
                            pattern_seen = True
                            if attached:
                                if flag in file_flags:
                                    positions[index] = 'attached'
                            elif flag in file_flags:
                                skip_next, skip_as_path = True, True
                            else:
                                skip_next, skip_as_path = True, False
                            consumed = True
                            break
                        if flag in value_flags and i + 1 == len(token):
                            skip_next, skip_as_path = True, False
                            consumed = True
                            break
                    if not consumed and '=' in token:
                        positions[index] = 'option'
        elif program in NON_PATH_PROGRAMS:
            pass
        elif program in GREP_FAMILY and not pattern_seen:
            pattern_seen = True
            mark_text_slot(index, token)
        else:
            positions[index] = 'operand'
        operand += 1
    return positions


def parse_link_operands(rest):
    """Targets and destination paths for `ln` / `cp -s` / `cp -l`."""
    args = rest[1:]
    dest_dir = None
    operands = []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "--":
            operands.extend(args[index + 1:])
            break
        if arg in ("-t", "--target-directory") and index + 1 < len(args):
            dest_dir = args[index + 1]
            index += 2
            continue
        if arg.startswith("--target-directory="):
            dest_dir = arg.split("=", 1)[1]
            index += 1
            continue
        if arg.startswith("-") and arg != "-" and not arg.startswith("--"):
            # `-t` as the last short flag takes the next argument.
            if arg.endswith("t") and "T" not in arg[1:]:
                if index + 1 < len(args):
                    dest_dir = args[index + 1]
                    index += 2
                    continue
            index += 1
            continue
        if arg.startswith("--"):
            index += 1
            continue
        operands.append(arg)
        index += 1
    if dest_dir is not None:
        return operands, [os.path.join(dest_dir, os.path.basename(target) or target) for target in operands], dest_dir
    if not operands:
        return [], [], None
    if len(operands) == 1:
        return operands, [os.path.basename(operands[0]) or operands[0]], None
    dest = operands[-1]
    targets = operands[:-1]
    if len(targets) == 1:
        return targets, [dest], None
    return targets, [os.path.join(dest, os.path.basename(target) or target) for target in targets], dest


def cp_creates_links(rest):
    for arg in rest[1:]:
        if arg == "--":
            break
        if arg in ("-s", "--symbolic-link", "-l", "--link"):
            return True
        if arg.startswith("-") and not arg.startswith("--") and ("s" in arg[1:] or "l" in arg[1:]):
            return True
    return False


def link_creation_touches_protected(rest, policy, current):
    program = os.path.basename(rest[0]) if rest else ""
    if program == "ln":
        pass
    elif program == "cp" and cp_creates_links(rest):
        pass
    else:
        return False
    targets, destinations, dest_dir = parse_link_operands(rest)
    cwd = current or policy["workspace_path"]
    for dest in destinations:
        dest_resolved = resolve_path(dest, policy, cwd)
        if within_conversation_scratch(dest_resolved, policy):
            return True
        if path_is_protected(dest, policy, cwd):
            return True
    parents = []
    if dest_dir is not None:
        parents.append(resolve_path(dest_dir, policy, cwd))
    elif len(destinations) == 1:
        dest_resolved = resolve_path(destinations[0], policy, cwd)
        parents.append(os.path.dirname(dest_resolved) or cwd)
    elif destinations:
        dest_resolved = resolve_path(destinations[0], policy, cwd)
        parents.append(os.path.dirname(dest_resolved) or cwd)
    for target in targets:
        for parent in [cwd] + parents:
            if path_is_protected(target, policy, parent):
                return True
    return False


def operand_touches_protected(candidate, policy, current, program, is_redirect, literal=None, segments=None):
    expanded = expand_exported(candidate, literal, segments, shell_token=True)
    if '$' in expanded or re.match(r"^~[^/]", expanded):
        # echo/printf argv text is data; a redirect target is always opened.
        if is_redirect or program not in PATH_TEXT_PROGRAMS:
            fail_closed("unresolved shell path expansion")
        return False
    if expanded == '~' or expanded.startswith('~/'):
        expanded = policy['process_home'] + expanded[1:]
    expanded = expanded.replace(chr(1), '$').replace(chr(3), '~')
    if not os.path.isabs(expanded):
        expanded = os.path.join(current or policy['workspace_path'], expanded)
    if any(c in expanded for c in '*?[') and glob_touches_protected(expanded, policy):
        return True
    return path_is_protected(expanded, policy, current, pre_expanded=True)


def command_touches_protected(command, policy, cwd):
    current = cwd
    groups = command_groups(command)
    reject_trusted_rebinding(command, groups)
    for group in groups:
        rest = strip_prefixes(group)
        inner = stdin_argv_program(group)
        if inner is not None and inner not in STDIN_ARGV_SAFE:
            fail_closed("stdin-to-argv runners cannot be inspected for protected paths")
        program = os.path.basename(rest[0]) if rest else ''
        if rest and link_creation_touches_protected(rest, policy, current):
            return True
        for body in python_inline_bodies(group):
            if python_body_touches_protected(body, policy):
                return True
        for index, role in sorted(path_operand_positions(group).items()):
            token = group[index]
            literal = token_literal(group, index)
            segments = token_segments(group, index)
            # Preserve quoted/escaped expansion characters during resolution.
            # True is single-quoted or escaped (literal $); double-quoted
            # characters are recorded as a distinct truthy marker so pattern
            # slots can tell unquoted expansions from "$X".
            protected_text = ''.join({'$': chr(1), '~': chr(3)}.get(c, c) if i < len(literal) and literal[i] is True else c
                                     for i, c in enumerate(token))
            is_redirect = role.startswith('redirect')
            if (is_redirect or program not in PATH_TEXT_PROGRAMS) and any(
                    char == '{' and not (i < len(literal) and literal[i]) and has_brace_expansion(token, literal, i)
                    for i, char in enumerate(token)):
                fail_closed("unresolved shell brace expansion")
            for candidate, offset in operand_path_texts(protected_text, role, program):
                cand_literal = literal[offset:offset + len(candidate)] if literal else []
                cand_segments = segments[offset:offset + len(candidate)] if segments is not None else None
                if candidate and not candidate.startswith('&') and operand_touches_protected(
                        candidate, policy, current, program, is_redirect, cand_literal, cand_segments):
                    return True
        if rest and rest[0] in ("cd", "pushd", "popd"):
            if len(rest) != 2 or rest[1] == "-" or re.match(r"^~[^/]", rest[1]) or any(c in rest[1] for c in "`*?["):
                return True
            rest_ids = strip_prefix_indices(group)
            arg_index = rest_ids[1] if len(rest_ids) > 1 else None
            arg_literal = token_literal(group, arg_index) if arg_index is not None else []
            arg_segments = token_segments(group, arg_index) if arg_index is not None else None
            expanded = expand_exported(rest[1], arg_literal, arg_segments, shell_token=True)
            if "$" in rest[1] and expanded == rest[1]:
                return True
            if "$" in expanded:
                return True
            current = resolve_path(expanded, policy, current, pre_expanded=True)
            if path_is_protected(current, policy, pre_expanded=True):
                return True
        for body in nested_shell_bodies(group):
            if command_touches_protected(body, policy, current):
                return True
    return False


def vcs_mutation(rest):
    if not rest:
        return False
    program = os.path.basename(rest[0])
    value_flags = {"-C", "-c", "-R", "--repo", "--repository", "--hostname", "-H", "--git-dir", "--work-tree", "--config", "--config-file", "--at-operation"}
    index = 1
    while index < len(rest) and rest[index].startswith("-"):
        index += 2 if rest[index] in value_flags else 1
    args = rest[index:]
    if not args:
        return False
    if program == "git":
        return args[0] in {"reset", "apply", "checkout", "restore", "commit", "stash", "clean", "merge", "rebase", "cherry-pick", "revert", "add", "rm", "mv"}
    if program == "jj":
        return args[0] in {"new", "describe", "squash", "restore", "abandon", "edit", "rebase", "commit", "split", "sparse", "undo", "revert", "bookmark"}
    if program == "gh" and args[0] == "api":
        for i, arg in enumerate(args[1:], 1):
            if arg in ("-f", "-F", "--field", "--raw-field", "--input") or arg.startswith(("-f", "-F", "--field=", "--raw-field=", "--input=")):
                return True
            if arg in ("-X", "--method") and i + 1 < len(args) and args[i + 1].upper() not in ("GET", "HEAD", "OPTIONS"):
                return True
            if arg not in ("-X", "--method") and arg.startswith(("-X", "--method=")) and arg.split("=", 1)[-1].removeprefix("-X").upper() not in ("GET", "HEAD", "OPTIONS"):
                return True
    return False


def is_recursive_rm(rest):
    if not rest or os.path.basename(rest[0]) != "rm":
        return False
    for token in rest[1:]:
        if token == "--recursive" or token == "-r" or token == "-R":
            return True
        if token.startswith("-") and not token.startswith("--") and "r" in token.lower():
            return True
    return False


NON_WRITING_KINDS = ("reviewer", "triage", "answer_agent", "review_guide", "progression")
SED_RANGE_RE = re.compile(r"[0-9$]+(?:,[0-9$]+)?p")


def read_only_sed(args, worker_kind):
    # Reviewer and triage never run sed. The other non-writing kinds may be
    # granted `sed -n`, which is only a reader for a plain line-range print.
    if worker_kind in ("reviewer", "triage"):
        return False
    if not args or args[0] != "-n":
        return False
    # GNU sed permutes options, so a trailing `-i` or `-e 'w out'` would still
    # write; every remaining argument must be a plain file operand.
    return len(args) >= 2 and SED_RANGE_RE.fullmatch(args[1]) is not None and not any(a.startswith("-") for a in args[2:])


def structural_command_reason(command, policy, worker_kind, cwd):
    for group in iter_groups(command):
        rest = strip_prefixes(group)
        normalized = without_redirections(group)
        prefixes = normalized[:len(normalized) - len(rest)]
        if any(os.path.basename(token) == "sudo" for token in prefixes):
            return "sudo is structurally denied for Antigravity workers"
        if not rest:
            continue
        base = os.path.basename(rest[0])
        if base == "sudo":
            return "sudo is structurally denied for Antigravity workers"
        if base == "bossctl":
            return "bossctl is coordinator-only and is structurally denied for workers"
        if is_recursive_rm(rest):
            return "destructive recursive removal is structurally denied"
    if command_touches_protected(command, policy, cwd):
        return "command accesses protected Antigravity or Boss state"
    if worker_kind in NON_WRITING_KINDS:
        if any(has_redirect(body) for body in inspected_commands(command)):
            return "%s workers must not perform shell-mediated writes" % (worker_kind,)
        for group in iter_groups(command):
            base = basename_of(group)
            if base in WRITE_PROGRAMS:
                return "%s workers must not run write command %s" % (worker_kind, base)
            rest = strip_prefixes(group)
            if not rest:
                continue
            prog = os.path.basename(rest[0])
            if prog in REVIEWER_BLOCKED or prog == "osascript" or prog.startswith("pypy") or re.fullmatch(r"python[0-9.]*", prog):
                return "%s workers must not execute uninspectable interpreter bodies" % worker_kind
            if vcs_mutation(rest):
                return "%s workers must not mutate repository or GitHub state" % worker_kind
            if prog == "find" and any(t == "-delete" or t.startswith(("-fprint", "-fls")) for t in rest[1:]):
                return "%s workers must not write find results" % worker_kind
            if prog == "sort" and any(t == "--output" or t.startswith(("-o", "--output=")) for t in rest[1:]):
                return "%s workers must not write sort output" % worker_kind
            if prog in ("patch", "awk", "gawk"):
                return "%s workers must not execute file-mutating text scripts" % worker_kind
            if prog == "sed" and not read_only_sed(rest[1:], worker_kind):
                return "%s workers must not execute file-mutating text scripts" % worker_kind
    return None


def manage_task_reason(native_name, args):
    if native_name != "manage_task":
        return None
    action = first_str(args, ("Action", "action", "TaskAction", "taskAction", "method", "Method"))
    blob = json.dumps(args)
    if action == "send_input" or "send_input" in blob:
        return "manage_task send_input is denied so stdin cannot become an uninspected command channel"
    return "manage_task is denied for Antigravity workers"


def structural_reason(native_name, args, canonical, policy):
    worker_kind = policy.get("worker_kind")
    if native_name == "browser_subagent":
        return "browser_subagent is structurally disabled until its tool path is qualified"
    manage = manage_task_reason(native_name, args)
    if manage:
        return manage
    if args.get("BypassSandbox") is True or canonical["tool_input"].get("BypassSandbox") is True:
        return "BypassSandbox is denied for Antigravity workers"
    if native_name not in KNOWN_TOOLS and not canonical["tool_name"].startswith("mcp__"):
        if native_name:
            return "unknown Antigravity tool %r is denied" % (native_name,)
        return "tool call is missing a name"
    protected = policy.get("protected_paths") or []
    if not isinstance(protected, list):
        protected = []
    protected = [p for p in protected if isinstance(p, str)]
    cwd = canonical.get("cwd")
    if isinstance(cwd, str) and path_is_protected(cwd, policy):
        return "working directory accesses protected Antigravity or Boss state"
    file_path = canonical["tool_input"].get("file_path")
    if isinstance(file_path, str) and path_is_protected(file_path, policy, canonical.get("cwd")):
        return "refusing access to protected Antigravity or Boss state: %s" % (file_path,)
    if worker_kind in ("reviewer", "triage") and native_name in ("write_file", "edit_file"):
        if (
            worker_kind == "reviewer"
            and native_name == "write_file"
            and isinstance(file_path, str)
            and is_artifact_write(file_path, policy, canonical.get("cwd"))
        ):
            return None
        return "%s workers must not modify files" % (worker_kind,)

    command = canonical["tool_input"].get("command")
    if isinstance(command, str):
        reason = structural_command_reason(command, policy, worker_kind, canonical.get("cwd"))
        if reason:
            return reason
    return None


def is_artifact_write(path, policy, cwd=None):
    allowed = list(policy.get("artifact_write_paths") or [])
    env_path = os.environ.get("BOSS_STRUCTURED_OUTPUT")
    if isinstance(env_path, str) and env_path and os.path.isabs(env_path):
        allowed.append(env_path)
    if not allowed:
        return False
    resolved = resolve_path(path, policy, cwd)
    for artifact in allowed:
        if not isinstance(artifact, str) or not artifact:
            continue
        if resolved == os.path.realpath(artifact):
            return True
    return False


def guard_applies(matcher, canonical_name):
    if matcher in ("*", ".*", ""):
        return True
    if matcher == "Bash":
        return canonical_name == "Bash"
    return matcher == canonical_name


def run_bounded(argv, **kwargs):
    data = kwargs.pop("input")
    timeout = kwargs.pop("timeout")
    kwargs.pop("capture_output", None)
    # The Rust probe owns a bounded process group. Keep probe descendants in
    # that group so its outer deadline also kills a hung forwarder or guard.
    new_session = os.environ.get("BOSS_AGY_PROBE") != "1"
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=new_session, **kwargs)
    try:
        stdout, stderr = child.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid if new_session else os.getpgrp(), signal.SIGKILL)
        child.communicate()
        raise
    return subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)


def run_guards(policy, canonical, rewrites=0, synthetic=False):
    commands = []
    bodies = set()
    stripped = set()
    if not synthetic and canonical.get("tool_name") == "Bash":
        original = canonical["tool_input"]["command"]
        for body in inspected_commands(original):
            if body != original:
                commands.append(body)
                bodies.add(body)
            for group in command_groups(body):
                normalized = without_redirections(group)
                if normalized and normalized != list(group):
                    form = shlex.join(normalized)
                    commands.append(form)
                    stripped.add(form)
        for command in commands:
            nested = dict(canonical)
            nested["tool_input"] = dict(canonical["tool_input"], command=command)
            # Tell guards that read redirections themselves that this form had
            # them removed on purpose (a form that is also a real nested body
            # keeps them).
            if command in stripped and command not in bodies:
                nested["redirects_stripped"] = True
            run_guards(policy, nested, rewrites, synthetic=True)
    guards = policy.get("guards")
    if not isinstance(guards, list) or not guards:
        fail_closed("authorization policy guards must be a nonempty array")
    canonical_name = canonical.get("tool_name")
    for guard in guards:
        if not isinstance(guard, dict):
            fail_closed("malformed guard entry in authorization policy")
        # Run the push gate once with all inspected forms. This preserves
        # nested/redirect-normalized push detection without repeating the gate.
        if synthetic and guard.get("run_once"):
            continue
        matcher = guard.get("matcher") or "*"
        if not guard_applies(matcher, canonical_name):
            continue
        command = guard.get("command")
        if not isinstance(command, str) or not command:
            fail_closed("guard entry is missing a command")
        guard_input = canonical
        if guard.get("run_once") and commands:
            # Join with a standalone ";" token: the guard splits command groups
            # only on standalone delimiter tokens and treats newlines as plain
            # whitespace, so a newline join would glue the forms onto the
            # first group of the original and hide a push from it.
            guard_input = dict(canonical)
            guard_input["tool_input"] = dict(canonical["tool_input"],
                command=" ; ".join(commands + [canonical["tool_input"]["command"]]))
        try:
            result = run_bounded(
                ["/bin/sh", "-c", command],
                input=json.dumps(guard_input),
                capture_output=True,
                text=True,
                timeout=guard.get("timeout_sec", 30),
            )
        except Exception as exc:
            fail_closed("failed to exec guard command: %r" % (exc,))
        if result.stderr:
            sys.stderr.write(result.stderr)
        if result.returncode != 0:
            fail_closed("guard exited nonzero or by signal: %s" % result.returncode)
        raw = result.stdout.strip()
        try:
            decision_obj = json.loads(raw) if raw else None
        except Exception:
            fail_closed("guard produced non-JSON stdout (exit=%s): %r" % (result.returncode, raw[:500]))
        if not isinstance(decision_obj, dict):
            fail_closed("guard produced no decision object (exit=%s)" % (result.returncode,))
        hook = decision_obj.get("hookSpecificOutput", {})
        if not isinstance(hook, dict):
            fail_closed("guard hookSpecificOutput is not an object")
        decision = decision_obj.get("decision", hook.get("permissionDecision"))
        rewrite = hook.get("updatedInput", decision_obj.get("updatedInput", decision_obj.get("overwrite")))
        if rewrite is not None:
            if decision in (None, "approve", "allow"):
                decision = "approve"
            else:
                rewrite = None
        reason = decision_obj.get("reason")
        if decision in ("block", "deny"):
            emit("deny", reason=reason or "guard denied the tool call")
        if decision in ("approve", "allow"):
            if rewrite is not None:
                if synthetic:
                    fail_closed("nested guard rewrites cannot be applied to the native command")
                before = dict(canonical["_native_args"])
                apply_rewrite(policy, canonical, rewrite)
                if before != canonical["_native_args"]:
                    if rewrites >= 4:
                        fail_closed("guard rewrite chain did not converge")
                    # Earlier guards must also approve the changed input.
                    run_guards(policy, canonical, rewrites + 1)
                    return
            continue
        fail_closed("guard emitted unrecognised decision %r (exit=%s)" % (decision, result.returncode))



def apply_rewrite(policy, canonical, rewrite):
    if not isinstance(rewrite, dict):
        fail_closed("guard updatedInput must be an object")
    aliases = {
        "command": ("CommandLine", "command", "commandLine"),
        "cwd": ("Cwd", "cwd", "workingDirectory"),
        "file_path": ("Path", "path", "TargetFile", "targetFile", "target_file", "file_path", "AbsolutePath", "absolutePath"),
        "content": ("Contents", "contents", "Content", "content"),
    }
    args = dict(canonical["_native_args"])
    args.update({k: v for k, v in rewrite.items() if k not in aliases})
    for key, names in aliases.items():
        if key in rewrite:
            for name in names:
                if name in args:
                    args[name] = rewrite[key]
            if not any(name in args for name in names):
                args[names[0]] = rewrite[key]
    native = canonical["agy_tool_name"]
    _, _, rewritten = canonicalise({"cwd": canonical.get("cwd"), "toolCall": {"name": native, "args": args}})
    reason = structural_reason(native, args, rewritten, policy)
    if reason:
        emit("deny", reason=reason)
    canonical.update(rewritten)
    canonical["_native_args"] = args



def forward_progress(raw_stdin, policy):
    forwarder = policy.get("forwarder")
    if not isinstance(forwarder, str) or not forwarder:
        fail_closed("authorization policy is missing the progress forwarder")
    if not os.path.isfile(forwarder) or not os.access(forwarder, os.X_OK):
        fail_closed("progress forwarder is missing or not executable: %s" % (forwarder,))
    env = os.environ.copy()
    if env.get("BOSS_AGY_PROBE") == "1":
        # Exercise the real forwarder against a private sink, including its
        # executable bit and transport, without publishing synthetic activity.
        temporary = tempfile.TemporaryDirectory(prefix="agy-probe-")
        sink = socket.socket(socket.AF_UNIX)
        sink.bind(temporary.name + "/events.sock")
        sink.listen(1)
        sink.settimeout(16)
        env.update(BOSS_EVENTS_SOCKET=temporary.name + "/events.sock",
                   BOSS_EVENT_SPOOL=temporary.name, BOSS_WORKSPACE=temporary.name)
        def drain():
            try:
                connection, _ = sink.accept()
                with connection:
                    while connection.recv(65536):
                        pass
            except OSError:
                pass
        threading.Thread(target=drain, daemon=True).start()
    try:
        result = run_bounded(
            [forwarder, "PreToolUse"],
            env=env,
            input=raw_stdin,
            capture_output=True,
            text=True,
            timeout=15,
        )
    except Exception as exc:
        fail_closed("progress forwarder failed to exec: %r" % (exc,))
    if result.stderr:
        sys.stderr.write(result.stderr)
    if result.returncode != 0:
        fail_closed(
            "progress forwarder exited %s; refusing to authorize after a delivery failure"
            % (result.returncode,)
        )


def authorize_payload(payload, policy, raw_stdin):
    if not isinstance(payload, dict):
        fail_closed("stdin payload was not a JSON object")
    verify_binding(policy)
    native_name, args, canonical = canonicalise(payload)
    canonical["_native_args"] = dict(args)
    # The conversation id comes from agy's own hook payload, never from the
    # tool arguments the model controls. It scopes the scratch allowance.
    policy["conversation_id"] = canonical.get("session_id")
    reason = structural_reason(native_name, args, canonical, policy)
    if reason:
        emit("deny", reason=reason)
    run_guards(policy, canonical)
    overwrite = {key: value for key, value in canonical["_native_args"].items() if key not in args or args[key] != value}
    authorized(policy, overwrite=overwrite)


def main():
    raw_stdin = sys.stdin.read()
    forward_progress(raw_stdin, {"forwarder": os.environ.get("BOSS_AGY_FORWARDER")})
    try:
        payload = json.loads(raw_stdin) if raw_stdin else None
    except Exception:
        fail_closed("stdin payload was not valid JSON: %r" % (raw_stdin[:500],))
    policy = load_policy()
    authorize_payload(payload, policy, raw_stdin)


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        fail_closed("authorization adapter failed: %s" % exc)
