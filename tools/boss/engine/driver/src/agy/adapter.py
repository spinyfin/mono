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


# COMMAND_TOKENIZER_FRAGMENT


def nested_shell_bodies(group):
    rest = strip_prefixes(group)
    if rest and os.path.basename(rest[0]) in ("sh", "bash", "zsh", "dash", "ksh"):
        for index, arg in enumerate(rest[1:], 1):
            if re.fullmatch(r"-[A-Za-z]*c[A-Za-z]*", arg) and index + 1 < len(rest):
                return [rest[index + 1]]
    if len(rest) >= 3 and os.path.basename(rest[0]) in ("python", "python3") and rest[1] == "-c":
        return [rest[2]]
    return []


def iter_groups(command, depth=0):
    if depth > 4:
        fail_closed("shell nesting exceeds the inspected depth")
    if any(group.error for group in command_records(command)):
        fail_closed("incomplete shell syntax cannot be inspected")
    for group in command_groups(command):
        yield group
        for body in nested_shell_bodies(group):
            for nested in iter_groups(body, depth + 1):
                yield nested


def basename_of(group):
    rest = strip_prefixes(group)
    if not rest:
        return ""
    return os.path.basename(rest[0])


def has_redirect(command):
    lexer = shlex.shlex(command, posix=False, punctuation_chars="><")
    lexer.whitespace_split = True
    tokens = list(lexer)
    for index, token in enumerate(tokens):
        if token in (">", ">>"):
            target = tokens[index + 1].strip(chr(34) + chr(39)) if index + 1 < len(tokens) else ""
            if target != "/dev/null":
                return True
    return False


def resolve_path(path, policy, cwd=None):
    home = policy["process_home"]
    path = path.replace(chr(36) + "{HOME}", home).replace("$HOME", home)
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
    # Enforcement leaf names also cover commands that change directory first,
    # and reads: disclosure of policy is as sensitive as modifying the hook.
    leaves = ("hooks.json", "session-settings.json", "boss-agy-auth-policy.json",
              "boss-agy-auth-attestation.json", "boss-agy-hook-adapter.py")
    for group in iter_groups(command):
        for token in group:
            if any(leaf in token for leaf in leaves):
                return True
            candidate = token.lstrip("<>")
            if path_is_protected(candidate, policy, cwd):
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
    # Substitutions execute before argv exists; fail closed instead of
    # pretending their contents are ordinary arguments.
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
            return "command substitution is structurally denied"
    for group in iter_groups(command):
        rest = strip_prefixes(group)
        prefixes = group[:len(group) - len(rest)]
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
        if has_redirect(command):
            return "%s workers must not perform shell-mediated writes" % (worker_kind,)
        for group in iter_groups(command):
            base = basename_of(group)
            if base in WRITE_PROGRAMS:
                return "%s workers must not run write command %s" % (worker_kind, base)
            rest = strip_prefixes(group)
            if not rest:
                continue
            prog = os.path.basename(rest[0])
            if vcs_mutation(rest):
                return "%s workers must not mutate repository or GitHub state" % worker_kind
            if prog in ("patch",) or (prog == "sed" and any(t == "--in-place" or t.startswith("-i") or t.startswith("--in-place=") for t in rest[1:])) or (prog in ("awk", "gawk") and any(t == "inplace" or t.startswith("--inplace") for t in rest[1:])):
                return "%s workers must not edit files in place" % worker_kind
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
    child = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True, **kwargs)
    try:
        stdout, stderr = child.communicate(data, timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.communicate()
        raise
    return subprocess.CompletedProcess(argv, child.returncode, stdout, stderr)


def run_guards(policy, canonical, rewrites=0):
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
    try:
        result = run_bounded(
            [forwarder, "PreToolUse"],
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
