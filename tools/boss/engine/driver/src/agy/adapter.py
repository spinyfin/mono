#!/usr/bin/env python3
"""Antigravity PreToolUse authorization adapter.

One entry point for every tool call: silently forward the raw payload to
boss-event, run the engine-owned guard chain, then print exactly one
verdict (auto_approve, deny, or deny_unless_prior_grant). Silent stdout
is not an authorization. Never emit allow, ask, or force_ask.

The engine probes this installed command with harmless sentinels and checks
content binding. Native SessionStart is not a pre-prompt readiness signal.
"""
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
    "grep",
    "glob",
    "list_dir",
}

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


def nested_shell_bodies(group):
    rest = strip_prefixes(group)
    if rest and any(c in rest[0] for c in "<>"):
        fail_closed("unsupported redirection attached to an executable")
    base = os.path.basename(rest[0]) if rest else ""
    if base in {"if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "case", "esac", "{", "}", "!", "eval", "source", "."}:
        fail_closed("unsupported shell control or dynamic execution")
    if rest and os.path.basename(rest[0]) in ("sh", "bash", "zsh", "dash", "ksh"):
        for index, arg in enumerate(rest[1:], 1):
            if re.fullmatch(r"-[A-Za-z]*c[A-Za-z]*", arg) and index + 1 < len(rest):
                return [rest[index + 1]]
        fail_closed("shell stdin or script execution cannot be inspected")
    if base in ("python", "python3"):
        if getattr(group, "body", ""):
            fail_closed("interpreter stdin bodies cannot be inspected")
        if len(rest) >= 3 and rest[1] == "-c":
            return [rest[2]]
    if base == "find":
        bodies = []
        for index, arg in enumerate(rest):
            if arg in ("-exec", "-execdir", "-ok", "-okdir"):
                end = next((i for i in range(index + 1, len(rest)) if rest[i] in (";", "+")), len(rest))
                bodies.append(shlex.join(rest[index + 1:end]))
        return bodies
    return []


def has_substitution(command):
    quote, escaped = None, False
    for index, char in enumerate(command):
        if escaped:
            escaped = False
            continue
        if char == chr(92) and quote != chr(39):
            escaped = True
            continue
        if char in (chr(34), chr(39)):
            if quote == char:
                quote = None
            elif quote is None:
                quote = char
        if quote != chr(39) and (char == chr(96) or command[index:index + 2] == "$("):
            return True
    return False


def inspected_commands(command, depth=0):
    if depth > 4:
        fail_closed("shell nesting exceeds the inspected depth")
    if any(group.error for group in command_records(command)):
        fail_closed("incomplete shell syntax cannot be inspected")
    if has_substitution(command):
        fail_closed("command substitution is structurally denied")
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


def resolve_path(path, policy, cwd=None):
    home = policy["process_home"]
    path = re.sub(r"\$(?:HOME\b|\{HOME\})", lambda _: home, path)
    if "$" in path or path.startswith(("~-", "~+")):
        fail_closed("unresolved shell path expansion")
    if path == "~" or path.startswith("~/"):
        path = home + path[1:]
    if not os.path.isabs(path):
        path = os.path.join(cwd or policy["workspace_path"], path)
    return os.path.realpath(path)


def path_is_protected(path, policy, cwd=None):
    expanded = resolve_path(path, policy, cwd)
    for root in policy.get("protected_paths", []):
        root_abs = os.path.realpath(root)
        if expanded == root_abs or expanded.startswith(root_abs + os.sep):
            return True
    return False


def command_touches_protected(command, policy, cwd):
    current = cwd
    for group in command_groups(command):
        rest = strip_prefixes(group)
        for token in group:
            candidate = token.split("=", 1)[1] if ASSIGNMENT_RE.match(token) else token
            candidate = re.sub(r"^(?:[0-9]+|&)?[<>]+", "", candidate)
            expanded = resolve_path(candidate, policy, current)
            if any(c in candidate for c in "*?["):
                static = re.split(r"[\*\?\[]", expanded, 1)[0].rstrip(os.sep)
                for root in policy["protected_paths"]:
                    root = os.path.realpath(root)
                    if root.startswith(static) or static.startswith(root + os.sep):
                        return True
            if path_is_protected(candidate, policy, current):
                return True
        if rest and rest[0] in ("cd", "pushd", "popd"):
            if len(rest) != 2 or rest[1] == "-" or rest[1].startswith(("~-", "~+")) or any(c in rest[1] for c in "$`*?["):
                return True
            current = resolve_path(rest[1], policy, current)
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
    if worker_kind in ("reviewer", "triage"):
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
            if prog in ("patch", "sed", "awk", "gawk"):
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
    file_path = canonical["tool_input"].get("file_path")
    if isinstance(file_path, str) and path_is_protected(file_path, policy, canonical.get("cwd")):
        return "refusing access to protected Antigravity or Boss state: %s" % (file_path,)
    if worker_kind in ("reviewer", "triage") and native_name in ("write_file", "edit_file"):
        return "%s workers must not modify files" % (worker_kind,)
    command = canonical["tool_input"].get("command")
    if isinstance(command, str):
        reason = structural_command_reason(command, policy, worker_kind, canonical.get("cwd"))
        if reason:
            return reason
    return None


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
    if not synthetic and canonical.get("tool_name") == "Bash":
        original = canonical["tool_input"]["command"]
        for body in inspected_commands(original):
            commands = [body] if body != original else []
            for group in command_groups(body):
                normalized = without_redirections(group)
                if normalized and normalized != list(group):
                    commands.append(shlex.join(normalized))
            for command in commands:
                nested = dict(canonical)
                nested["tool_input"] = dict(canonical["tool_input"], command=command)
                run_guards(policy, nested, rewrites, synthetic=True)
    guards = policy.get("guards")
    if not isinstance(guards, list) or not guards:
        fail_closed("authorization policy guards must be a nonempty array")
    canonical_name = canonical.get("tool_name")
    for guard in guards:
        if not isinstance(guard, dict):
            fail_closed("malformed guard entry in authorization policy")
        matcher = guard.get("matcher") or "*"
        if not guard_applies(matcher, canonical_name):
            continue
        command = guard.get("command")
        if not isinstance(command, str) or not command:
            fail_closed("guard entry is missing a command")
        try:
            result = run_bounded(
                ["/bin/sh", "-c", command],
                input=json.dumps(canonical),
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
