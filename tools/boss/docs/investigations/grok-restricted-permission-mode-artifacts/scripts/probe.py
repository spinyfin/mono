#!/usr/bin/env python3
"""Drive a Boss-shaped interactive `grok` TUI in a pty and print its hook stream.

usage: probe.py <label> <posture> <prompt>

`posture` is read as tokens, in any order:

  progression | standard   which rule set: the progression allowlist and deny
                           belt, or the ordinary structural denies only
  noaa                     omit --always-approve
  settings                 write permissions.defaultMode = dontAsk into the
                           scoped HOME's .claude/settings.json, and omit the
                           --permission-mode flag
  flag                     keep --permission-mode dontAsk alongside `settings`
  hook                     arm the PreToolUse guard that redirects $BOSS_BIN

PROBE_EXTRA_ALLOW (a JSON list) adds --allow rules to the progression set, for
trying rule spellings. PROBE_CONFIG_TOML names a config.toml to copy into the
throwaway GROK_HOME (the engine's own, to probe under its compat settings).
PROBE_GUARD_HOOK is a complete hook command to arm as a Bash PreToolUse guard
in place of the built-in stand-in when `hook` is given (the driver's adapter
and guard, to probe the installed pair).

Runs land under $PROBE_RUNS (default: ~/.cache/grok-restricted-mode-probe).
Keep that path short: GROK_HOME holds a unix socket. Each probe spends one
short model turn on the host's Grok credential, read through GROK_AUTH_PATH.
"""
import fcntl, json, os, pty, re, select, shutil, signal, struct, sys, termios, time, uuid

RUNS = os.environ.get("PROBE_RUNS") or os.path.expanduser("~/.cache/grok-restricted-mode-probe")


def publish_deny():
    out = []
    for c in ["jj git push", "git push", "gh pr create", "gh pr merge", "gh pr close", "gh pr edit",
              "gh pr comment", "gh pr review", "gh issue create", "gh issue comment", "gh issue close",
              "gh issue edit", "cube pr", '"$CUBE_BIN" pr', "$CUBE_BIN pr"]:
        out += [f"Bash({c})", f"Bash({c}:*)"]
    return out


def read_only_deny():
    out = ["Edit(**)", "NotebookEdit(**)"] + publish_deny()
    for c in ["cube", '"$CUBE_BIN"', "$CUBE_BIN"]:
        out += [f"Bash({c})", f"Bash({c}:*)"]
    return out


def progression_deny():
    out = read_only_deny()
    for v in ["jj", "git"]:
        out += [f"Bash({v})", f"Bash({v}:*)"]
    for verb in ["pr ready", "pr reopen", "pr lock", "pr unlock", "issue reopen", "issue delete",
                 "issue transfer", "issue pin", "issue unpin", "issue lock", "issue unlock"]:
        out.append(f"Bash(gh {verb}:*)")
    out += ["Bash(gh api)", "Bash(gh api:*)"]
    return out


def progression_allow():
    out = ["Read", "Grep", "Glob"]
    for b in ["boss", '"$BOSS_BIN"']:
        for sub in ["propose", "task show", "task list", "pr status"]:
            out.append(f"Bash({b} {sub}:*)")
    out += ["Bash(gh pr view:*)", "Bash(gh pr checks:*)"]
    return out


def structural_deny(data_dir):
    out = []
    for p in ["Read", "Edit"]:
        out += [f"{p}({data_dir})", f"{p}({data_dir}/**)"]
    out += ["Bash(rm -rf *)", "Bash(rm -rf:*)", "Bash(sudo)", "Bash(sudo:*)", "Bash(bossctl)", "Bash(bossctl:*)"]
    return out


def extra_args(posture, data_dir):
    args = ["--sandbox", "off"]
    denies = structural_deny(data_dir)
    if posture.startswith("progression"):
        for r in progression_deny():
            if r not in denies:
                denies.append(r)
    for r in denies:
        args += ["--deny", r]
    if posture.startswith("progression"):
        for r in progression_allow() + json.loads(os.environ.get("PROBE_EXTRA_ALLOW", "[]")):
            args += ["--allow", r]
        if ("settings" not in posture and "nomode" not in posture) or "flag" in posture:
            args += ["--permission-mode", "dontAsk"]
    return args


DUMP = r'''
import json, os, sys
log = sys.argv[1]
raw = sys.stdin.read()
try:
    payload = json.loads(raw) if raw.strip() else None
except Exception:
    payload = {"__unparseable__": raw[:2000]}
with open(log, "a") as f:
    import time
    f.write(json.dumps({"t": time.time(), "event": os.environ.get("GROK_HOOK_EVENT"), "payload": payload}) + "\n")
'''

GUARD = r'''
import json, re, sys
payload = json.load(sys.stdin)
cmd = (payload.get("toolInput") or {}).get("command") or ""
if payload.get("toolName") in ("run_terminal_command", "Bash") and re.search(r"\$\{?BOSS_BIN\b", cmd):
    print(json.dumps({"decision": "deny", "reason": "This session cannot pre-approve a command whose program is a shell variable. Re-run the same command with the literal word boss in place of \"$BOSS_BIN\" (boss is on PATH and is the same CLI)."}))
    sys.exit(0)
'''


def main():
    label, posture, prompt = sys.argv[1], sys.argv[2], sys.argv[3]
    run = os.path.join(RUNS, label)
    shutil.rmtree(run, ignore_errors=True)
    home, phome, cwd = (os.path.join(run, d) for d in ("gh", "ph", "cwd"))
    for d in (os.path.join(home, "hooks"), phome, cwd, os.path.join(run, "bin")):
        os.makedirs(d)
    log = os.path.join(run, "hooks.jsonl")
    seen, paths = set(), []
    for p in (cwd, os.path.realpath(cwd), "/tmp", "/private/tmp"):
        if p not in seen:
            seen.add(p); paths.append(p)
    with open(os.path.join(home, "trusted_folders.toml"), "w") as f:
        f.write("".join(f'[folders."{p}"]\ntrusted = true\ndecided_at = 1785000000\n\n' for p in paths))
    open(os.path.join(home, "hooks", "dump.py"), "w").write(DUMP)
    open(os.path.join(home, "hooks", "guard.py"), "w").write(GUARD)
    hook = {"matcher": "*", "hooks": [{"type": "command",
            "command": f"python3 {home}/hooks/dump.py {log}", "timeout": 15}]}
    events = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure",
              "PermissionDenied", "Stop", "StopFailure", "Notification", "SessionEnd"]
    hooks = {e: [hook] for e in events}
    if "hook" in posture and os.environ.get("PROBE_GUARD_HOOK"):
        hooks["PreToolUse"].append({"matcher": "Bash", "hooks": [{"type": "command",
            "command": os.environ["PROBE_GUARD_HOOK"]}]})
    elif "hook" in posture:
        hooks["PreToolUse"].append({"matcher": "*", "hooks": [{"type": "command",
            "command": f"python3 {home}/hooks/guard.py", "timeout": 15}]})
    json.dump({"hooks": hooks}, open(os.path.join(home, "hooks", "probe.json"), "w"), indent=2)
    # A harmless stand-in `boss` so allowed commands have something to execute.
    boss = os.path.join(run, "bin", "boss")
    open(boss, "w").write('#!/bin/sh\necho "PROBE_BOSS_RAN $*"\n')
    os.chmod(boss, 0o755)

    if os.environ.get("PROBE_CONFIG_TOML"):
        shutil.copy(os.environ["PROBE_CONFIG_TOML"], os.path.join(home, "config.toml"))
    sid = str(uuid.uuid4())
    argv = ["grok", "--model", "grok-4.6", "--reasoning-effort", "low", "--no-alt-screen"]
    if "noaa" not in posture:
        argv.append("--always-approve")
    if "settings" in posture:
        os.makedirs(os.path.join(phome, ".claude"))
        json.dump({"permissions": {"defaultMode": "dontAsk"}}, open(os.path.join(phome, ".claude", "settings.json"), "w"))
    argv += ["--trust", "--session-id", sid, "--cwd", cwd, "--no-subagents", "--no-memory"]
    argv += extra_args(posture, os.path.join(run, "fake-boss-data"))
    argv.append(prompt)
    open(os.path.join(run, "argv.json"), "w").write(json.dumps(argv, indent=1))
    env = {
        "PATH": os.path.join(run, "bin") + ":/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:" + os.path.expanduser("~/.local/bin"),
        "HOME": phome, "GROK_HOME": home,
        "GROK_AUTH_PATH": os.path.expanduser("~/.grok/auth.json"),
        "GROK_DISABLE_API_KEY_AUTH": "1",
        "TERM": "xterm-256color", "BOSS_BIN": boss,
    }
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execvpe("grok", argv, env)
        os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))
    raw = open(os.path.join(run, "pane.raw"), "wb")
    deadline = time.time() + float(os.environ.get("PROBE_TIMEOUT", "150"))
    settle_at = None

    def state():
        evs = []
        if os.path.exists(log):
            for line in open(log):
                try:
                    evs.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
        return evs

    outcome = "timeout"
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 1.0)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                break
            if not d:
                break
            raw.write(d); raw.flush()
        evs = state()
        names = [e["event"] for e in evs]
        prompted = any(e["event"] == "notification" and (e["payload"] or {}).get("notificationType") == "permission_prompt" for e in evs)
        if settle_at is None and "stop" in names:
            outcome, settle_at = "turn_ended", time.time() + 3
        if settle_at is None and prompted:
            outcome, settle_at = "PERMISSION_PROMPT", time.time() + float(os.environ.get("PROBE_HOLD", "12"))
        if settle_at and time.time() > settle_at:
            break
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.kill(pid, sig)
        except ProcessLookupError:
            break
        end = time.time() + 4
        while time.time() < end:
            w, _ = os.waitpid(pid, os.WNOHANG)
            if w:
                break
            time.sleep(0.1)
        else:
            continue
        break
    raw.close()
    summary = {"label": label, "posture": posture, "prompt": prompt, "outcome": outcome, "events": []}
    t0 = (state() or [{"t": 0}])[0].get("t", 0)
    for e in state():
        p = e["payload"] or {}
        row = {"t": round(e.get("t", 0) - t0, 2), "event": e["event"]}
        for k in ("toolName", "notificationType", "message", "permissionMode", "reason", "error"):
            if p.get(k) is not None:
                row[k] = p[k]
        ti = p.get("toolInput") or {}
        if ti.get("command"):
            row["command"] = ti["command"]
        tr = p.get("toolResponse") or p.get("toolResult")
        if tr is not None:
            row["toolResponse"] = json.dumps(tr)[:400]
        summary["events"].append(row)
    text = open(os.path.join(run, "pane.raw"), "rb").read().decode("utf-8", "replace")
    text = re.sub(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07|\x1b[()][A-B0-2]", "", text)
    lines = [l.rstrip() for l in text.replace("\r", "\n").split("\n") if l.strip()]
    summary["pane_tail"] = lines[-30:]
    json.dump(summary, open(os.path.join(run, "summary.json"), "w"), indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
