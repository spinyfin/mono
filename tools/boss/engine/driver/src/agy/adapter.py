#!/usr/bin/env python3
"""Antigravity PreToolUse authorization adapter.

One entry point for every tool call: silently forward the raw payload to
boss-event, run the engine-owned guard chain, then print exactly one
verdict (auto_approve, deny, or deny_unless_prior_grant). Silent stdout
is not an authorization. Never emit allow, ask, or force_ask.

`--probe` exercises the same path with harmless sentinels and checks
content binding. Native SessionStart is not a pre-prompt readiness signal.
"""
import hashlib
import json
import os
import re
import shlex
import subprocess
import sys

POSITIVE_SENTINEL = "echo boss-agy-auth-positive"
DENIED_SENTINEL = "sudo rm -rf /tmp/boss-agy-auth-denied-sentinel"

TOOL_NAME_MAP = {
    "run_command": "Bash",
    "write_file": "Write",
    "edit_file": "Edit",
    "read_file": "Read",
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
    "list_directory",
    "list_files",
    "search_codebase",
    "view_file",
    "read_url",
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
    if not env_run and "--probe" not in sys.argv:
        fail_closed("BOSS_RUN_ID is unset; refusing to authorize without run identity")
    approval = policy.get("approval")
    if approval not in ("auto_approve", "deny_unless_prior_grant"):
        fail_closed("authorization policy has unknown approval %r" % (approval,))
    return policy


def verify_binding(policy, require_self=True):
    attestation_path = policy.get("attestation_path")
    if not isinstance(attestation_path, str) or not attestation_path:
        fail_closed("authorization policy is missing attestation_path")
    attestation = load_json_file(attestation_path, "authorization attestation")
    entries = attestation.get("entries")
    if not isinstance(entries, list) or not entries:
        fail_closed("authorization attestation has no entries")
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
        if isinstance(args, dict):
            return args
    for key in ("tool_input", "toolInput"):
        args = payload.get(key)
        if isinstance(args, dict):
            return args
    return {}


def first_str(args, keys):
    for key in keys:
        value = args.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def canonicalise(payload):
    native_name = tool_name_of(payload)
    args = tool_args_of(payload)
    canonical_name = TOOL_NAME_MAP.get(native_name, native_name)
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
    if isinstance(cwd_payload, str):
        out["cwd"] = cwd_payload
    elif cwd is not None:
        out["cwd"] = cwd
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


def command_groups(command):
    try:
        tokens = shlex.split(command, posix=True)
    except Exception:
        tokens = command.split()
    groups = []
    current = []
    for token in tokens:
        if token in ("&&", "||", ";", "|", "&"):
            if current:
                groups.append(current)
                current = []
        else:
            current.append(token)
    if current:
        groups.append(current)
    return groups


def strip_prefixes(group):
    launchers = {
        "env",
        "nohup",
        "nice",
        "ionice",
        "stdbuf",
        "timeout",
        "time",
        "command",
        "exec",
        "busybox",
    }
    index = 0
    while index < len(group):
        token = group[index]
        if re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", token):
            index += 1
            continue
        base = os.path.basename(token)
        if base in launchers:
            index += 1
            if base == "timeout" and index < len(group):
                index += 1
            continue
        break
    return group[index:]


def nested_shell_bodies(group):
    rest = strip_prefixes(group)
    if len(rest) >= 3 and os.path.basename(rest[0]) in ("sh", "bash", "zsh", "dash", "ksh") and rest[1] == "-c":
        return [rest[2]]
    if len(rest) >= 3 and os.path.basename(rest[0]) in ("python", "python3") and rest[1] == "-c":
        return [rest[2]]
    return []


def iter_groups(command, depth=0):
    if depth > 4:
        return
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
    return bool(re.search(r"(^|[^0-9])>|>>", command))


def path_is_protected(path, protected):
    if not path:
        return False
    expanded = os.path.abspath(os.path.expanduser(path))
    for root in protected:
        if not root:
            continue
        root_abs = os.path.abspath(root)
        if expanded == root_abs or expanded.startswith(root_abs + os.sep):
            return True
    return False


def command_touches_protected(command, protected):
    for root in protected:
        if root and root in command:
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


def structural_command_reason(command, policy, worker_kind):
    protected = policy.get("protected_paths") or []
    if not isinstance(protected, list):
        protected = []
    protected = [p for p in protected if isinstance(p, str)]
    for group in iter_groups(command):
        rest = strip_prefixes(group)
        if not rest:
            continue
        base = os.path.basename(rest[0])
        if base == "sudo":
            return "sudo is structurally denied for Antigravity workers"
        if base == "bossctl":
            return "bossctl is coordinator-only and is structurally denied for workers"
        if is_recursive_rm(rest):
            return "destructive recursive removal is structurally denied"
    if command_touches_protected(command, protected) and (
        has_redirect(command) or any(basename_of(group) in WRITE_PROGRAMS for group in iter_groups(command))
    ):
        return "command writes into protected Antigravity or Boss state"
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
            if prog == "git" and "push" in rest:
                return "%s workers must not publish" % (worker_kind,)
            if prog == "jj" and "push" in rest:
                return "%s workers must not publish" % (worker_kind,)
            if prog == "gh" and len(rest) >= 3 and rest[1] in ("pr", "issue"):
                return "%s workers must not mutate GitHub" % (worker_kind,)
            if prog == "cube" and len(rest) >= 2 and rest[1] == "pr":
                return "%s workers must not publish" % (worker_kind,)
    return None


def mcp_reason(native_name, args):
    if native_name == "call_mcp_tool" or native_name.startswith("mcp_"):
        server = first_str(args, ("ServerName", "serverName", "server", "Server"))
        tool = first_str(args, ("ToolName", "toolName", "tool", "Tool"))
        detail = native_name
        if server or tool:
            detail = "%s server=%s tool=%s" % (native_name, server or "?", tool or "?")
        return "MCP tool calls are denied for Antigravity workers (%s)" % (detail,)
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
    mcp = mcp_reason(native_name, args)
    if mcp:
        return mcp
    manage = manage_task_reason(native_name, args)
    if manage:
        return manage
    if args.get("BypassSandbox") is True or canonical["tool_input"].get("BypassSandbox") is True:
        return "BypassSandbox is denied for Antigravity workers"
    if native_name not in KNOWN_TOOLS and native_name not in TOOL_NAME_MAP:
        if native_name:
            return "unknown Antigravity tool %r is denied" % (native_name,)
        return "tool call is missing a name"
    protected = policy.get("protected_paths") or []
    if not isinstance(protected, list):
        protected = []
    protected = [p for p in protected if isinstance(p, str)]
    file_path = canonical["tool_input"].get("file_path")
    if isinstance(file_path, str) and path_is_protected(file_path, protected):
        return "refusing access to protected Antigravity or Boss state: %s" % (file_path,)
    if worker_kind in ("reviewer", "triage") and native_name in ("write_file", "edit_file"):
        return "%s workers must not modify files" % (worker_kind,)
    command = canonical["tool_input"].get("command")
    if isinstance(command, str):
        reason = structural_command_reason(command, policy, worker_kind)
        if reason:
            return reason
    return None


def guard_applies(matcher, canonical_name):
    if matcher in ("*", ".*", ""):
        return True
    if matcher == "Bash":
        return canonical_name == "Bash"
    return matcher == canonical_name


def run_guards(policy, canonical):
    guards = policy.get("guards") or []
    if not isinstance(guards, list):
        fail_closed("authorization policy guards must be an array")
    canonical_name = canonical.get("tool_name")
    stdin = json.dumps(canonical)
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
            result = subprocess.run(
                ["/bin/sh", "-c", command],
                input=stdin,
                capture_output=True,
                text=True,
            )
        except Exception as exc:
            fail_closed("failed to exec guard command: %r" % (exc,))
        if result.stderr:
            sys.stderr.write(result.stderr)
        raw = result.stdout.strip()
        try:
            decision_obj = json.loads(raw) if raw else None
        except Exception:
            fail_closed("guard produced non-JSON stdout (exit=%s): %r" % (result.returncode, raw[:500]))
        if not isinstance(decision_obj, dict):
            fail_closed("guard produced no decision object (exit=%s)" % (result.returncode,))
        decision = decision_obj.get("decision")
        reason = decision_obj.get("reason")
        if decision in ("block", "deny"):
            emit("deny", reason=reason or "guard denied the tool call")
        if decision in ("approve", "allow"):
            continue
        fail_closed("guard emitted unrecognised decision %r (exit=%s)" % (decision, result.returncode))


def forward_progress(raw_stdin, policy):
    if os.environ.get("BOSS_AGY_SKIP_FORWARD") == "1" or "--probe" in sys.argv:
        return
    forwarder = policy.get("forwarder")
    if not isinstance(forwarder, str) or not forwarder:
        fail_closed("authorization policy is missing the progress forwarder")
    if not os.path.isfile(forwarder) or not os.access(forwarder, os.X_OK):
        fail_closed("progress forwarder is missing or not executable: %s" % (forwarder,))
    try:
        result = subprocess.run(
            [forwarder],
            input=raw_stdin,
            capture_output=True,
            text=True,
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
    forward_progress(raw_stdin, policy)
    reason = structural_reason(native_name, args, canonical, policy)
    if reason:
        emit("deny", reason=reason)
    run_guards(policy, canonical)
    authorized(policy)


def sentinel_payload(command):
    return {
        "hookEventName": "PreToolUse",
        "conversationId": "probe",
        "toolCall": {"id": "probe", "name": "run_command", "args": {"CommandLine": command, "Cwd": "/tmp"}},
    }


def capture_authorize(payload, policy):
    raw = json.dumps(payload)
    env = os.environ.copy()
    env["BOSS_AGY_SKIP_FORWARD"] = "1"
    env["BOSS_RUN_ID"] = policy.get("run_id") or env.get("BOSS_RUN_ID") or "probe"
    env["BOSS_AGY_POLICY"] = policy_path()
    result = subprocess.run(
        [sys.executable, os.path.abspath(__file__)],
        input=raw,
        capture_output=True,
        text=True,
        env=env,
    )
    stdout = result.stdout.strip()
    try:
        body = json.loads(stdout) if stdout else {}
    except Exception:
        body = {"decision": "deny", "reason": "probe child produced non-JSON: %r" % (stdout[:500],)}
    return body, result.stderr


def run_probe():
    os.environ["BOSS_AGY_SKIP_FORWARD"] = "1"
    policy = load_policy()
    if not os.environ.get("BOSS_RUN_ID"):
        os.environ["BOSS_RUN_ID"] = policy["run_id"]
    attestation = verify_binding(policy)
    expected = policy["approval"]
    positive, _ = capture_authorize(sentinel_payload(POSITIVE_SENTINEL), policy)
    denied, _ = capture_authorize(sentinel_payload(DENIED_SENTINEL), policy)
    binding_ok = True
    positive_ok = positive.get("decision") == expected
    denied_ok = denied.get("decision") == "deny"
    ok = binding_ok and positive_ok and denied_ok
    report = {
        "ok": ok,
        "binding": {"ok": True, "entries": attestation.get("entries")},
        "positive": {
            "decision": positive.get("decision"),
            "reason": positive.get("reason"),
            "sentinel": POSITIVE_SENTINEL,
            "expected": expected,
        },
        "denied": {
            "decision": denied.get("decision"),
            "reason": denied.get("reason"),
            "sentinel": DENIED_SENTINEL,
        },
    }
    if not ok:
        report["reason"] = "pre-prompt authorization probe failed"
        if not positive_ok:
            report["reason"] = "positive sentinel did not receive %s (got %r)" % (
                expected,
                positive.get("decision"),
            )
        elif not denied_ok:
            report["reason"] = "denied sentinel was not denied (got %r)" % (denied.get("decision"),)
    sys.stdout.write(json.dumps(report))
    sys.exit(0 if ok else 1)


def main():
    if "--probe" in sys.argv:
        run_probe()
        return
    raw_stdin = sys.stdin.read()
    try:
        payload = json.loads(raw_stdin) if raw_stdin else None
    except Exception:
        fail_closed("stdin payload was not valid JSON: %r" % (raw_stdin[:500],))
    policy = load_policy()
    authorize_payload(payload, policy, raw_stdin)


if __name__ == "__main__":
    main()
