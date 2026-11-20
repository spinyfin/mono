"""Dated record (2026-11-18, Grok CLI 1.0.70) of how the Grok spawn latency
investigation was measured. Not a supported tool: it models the engine by hand
and will drift from it, so do not rerun it to measure the current engine.

Time what a Grok worker spawn costs outside the engine.

Usage:
  python3 /absolute/path/grok_spawn_latency_probe.py preflight WORKSPACE SCRATCH RUNS [--parallel N]
  python3 /absolute/path/grok_spawn_latency_probe.py tui WORKSPACE SCRATCH RUNS [--brief-bytes N] [--hold EVENT:SECONDS]

WORKSPACE is a checkout of this repository that this session owns. SCRATCH is
a throwaway directory; every run gets a fresh GROK_HOME and process HOME under
it, as each engine spawn does. Results are appended to SCRATCH/preflight.jsonl
or SCRATCH/tui.jsonl and summarised on stdout.

`preflight` runs the subprocesses Grok provisioning runs before a pane exists,
in the engine's order and environment (`engine/driver/src/grok/home.rs`,
`engine/driver/src/grok/preflight.rs`).

`tui` runs the repository's brief launcher around the engine's `grok` command
line in a private, detached tmux server, with hooks that only record a
timestamp. It stops the pane as soon as `UserPromptSubmit` fires, so the brief
is accepted and no turn runs to completion. Seconds are measured from creating
the tmux session.

`setup` times the local steps around those subprocesses: writing a Grok home
(config, trust file, hook file, delegated paths), creating a detached tmux
session, the launcher's exact first-success Python import check, and Codex's
bundled-model probe. The real Rust setup paths, including composition and pane
title, are measured separately by `//tools/boss/engine/core:spawn_setup_probe`.

The CONFIG_TOML, CANARY_HOOKS, HOOK_EVENTS, LAUNCHER_CONFIG and trust_variants
definitions are a snapshot copied by hand from `engine/driver/src/grok/home.rs`,
`hooks.rs` and `grok.rs` as of Grok CLI 1.0.70 (2026-11-18). Nothing ties them
to the source: re-sync them before reusing this probe on a later revision.

`preflight` runs each command directly and sums `subprocess.run` durations. It
does not run the engine's own preflight, so it excludes the wait for a live
OAuth refresh lock (`engine/driver/src/grok/environment.rs`) and the reprobe
after a credential refresh.

Nothing here talks to a Boss engine. It uses the host's shared Grok login
through GROK_AUTH_PATH, without copying the credential.
"""

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import shlex
import shutil
import statistics
import subprocess
import time
import uuid

HOST_HOME = Path(os.environ["HOME"])
LAUNCHER = Path(__file__).resolve().parents[2] / "engine/driver/src/initial_prompt.py"
BUNDLED_CUBE = os.environ.get("PROBE_CUBE", "/Applications/Boss.app/Contents/Resources/bin/cube")

# `render_base_config_toml` and `render_provision_hooks_json` in home.rs.
CONFIG_TOML = """[compat.claude]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[compat.cursor]
hooks = false
agents = false
skills = false
mcps = false
rules = false
sessions = false

[ui]
vim_mode = false
"""
CANARY_HOOKS = {"hooks": {"SessionStart": [{"matcher": "*", "hooks": [{"type": "command", "command": "true"}]}]}}
# `GROK_HOOK_EVENTS` in hooks.rs.
HOOK_EVENTS = ["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Stop", "Notification", "SessionEnd"]
# `GrokDriver::pane_monitor_spec` and `launcher_starting_markers` in grok.rs.
LAUNCHER_CONFIG = {
    "blocking_dialog_markers": [],
    "agent_markers": ["Shift+Tab:mode", "always-approve", "Grok 4"],
    "starting_markers": ["Starting session", "Connecting...", "Waiting for approval", "Approve in your browser"],
    "paste_landed_markers": [],
    "post_submit_failure_markers": [],
    "check_settings_log": False,
}
# The variables `apply_tool_sandbox_environment` removes for cube, gh and jj.
PARENT_ONLY = [
    "GH_CONFIG_DIR", "CUBE_DATA_DIR", "CUBE_CONFIG_DIR", "JJ_CONFIG", "GIT_CONFIG_GLOBAL",
    "XDG_CACHE_HOME", "REPOBIN_CACHE_DIR", "BAZELISK_HOME", "TEST_TMPDIR",
]


def trust_variants(path):
    text = str(path)
    variants = {text}
    for short, long in (("/tmp", "/private/tmp"), ("/var", "/private/var")):
        if text.startswith(short + "/"):
            variants.add(long + text[len(short):])
        if text.startswith(long + "/"):
            variants.add(short + text[len(long):])
    return sorted(variants)


def delegate(source, destination):
    if source.exists() and not destination.is_symlink():
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.symlink_to(source)


def provision(container, workspace):
    """The scoped homes and environment `provision_grok_home` builds."""
    grok_home, process_home = container / "grok-home", container / "process-home"
    (grok_home / "hooks").mkdir(parents=True)
    (grok_home / "sessions").mkdir()
    process_home.mkdir()
    (grok_home / "config.toml").write_text(CONFIG_TOML)
    decided_at = int(time.time())
    (grok_home / "trusted_folders.toml").write_text("".join(
        f'[folders."{path}"]\ntrusted = true\ndecided_at = {decided_at}\n\n' for path in trust_variants(workspace)))
    (grok_home / "hooks/boss-provision.json").write_text(json.dumps(CANARY_HOOKS))
    jj_config = HOST_HOME / ".jjconfig.toml"
    if not jj_config.is_file():
        jj_config = HOST_HOME / "Library/Application Support/jj/config.toml"
    for source, leaf in (
        (HOST_HOME / ".config/gh", ".config/gh"),
        (HOST_HOME / ".local/share/cube", ".local/share/cube"),
        (HOST_HOME / ".config/cube", ".config/cube"),
        (HOST_HOME / "Library/Keychains/login.keychain-db", "Library/Keychains/login.keychain-db"),
        (jj_config, "Library/Application Support/jj/config.toml"),
        (HOST_HOME / ".gitconfig", ".gitconfig"),
        (HOST_HOME / ".ssh", ".ssh"),
        (HOST_HOME / ".bazelrc", ".bazelrc"),
        (HOST_HOME / ".cache/repobin", ".cache/repobin"),
        (HOST_HOME / "Library/Caches/bazelisk", "Library/Caches/bazelisk"),
        (HOST_HOME / "Library/Caches/bazel", "Library/Caches/bazel"),
        (HOST_HOME / ".cache/bazelcache", ".cache/bazelcache"),
    ):
        delegate(source, process_home / leaf)
    directives = {
        "GROK_HOME": str(grok_home),
        "GROK_AUTH_PATH": str(HOST_HOME / ".grok/auth.json"),
        "HOME": str(process_home),
        "GH_CONFIG_DIR": str(HOST_HOME / ".config/gh"),
        "CUBE_DATA_DIR": str(HOST_HOME / ".local/share/cube"),
        "CUBE_CONFIG_DIR": str(HOST_HOME / ".config/cube"),
        "XDG_CACHE_HOME": str(HOST_HOME / ".cache"),
        "REPOBIN_CACHE_DIR": str(HOST_HOME / ".cache/repobin"),
        "BAZELISK_HOME": str(HOST_HOME / "Library/Caches/bazelisk"),
        "TEST_TMPDIR": str(HOST_HOME / "Library/Caches/bazel"),
        "GROK_DISABLE_API_KEY_AUTH": "1",
    }
    if jj_config.is_file():
        directives["JJ_CONFIG"] = str(jj_config)
    if (HOST_HOME / ".gitconfig").is_file():
        directives["GIT_CONFIG_GLOBAL"] = str(HOST_HOME / ".gitconfig")
    return grok_home, directives, ["XAI_API_KEY", "GROK_AUTH", "GROK_FOLDER_TRUST"]


def preflight_once(index, workspace, scratch):
    container = scratch / f"preflight-{index:03d}-{uuid.uuid4().hex[:6]}"
    grok_home, directives, unsets = provision(container, workspace)
    grok_env = {key: value for key, value in os.environ.items() if key not in unsets} | directives
    tool_env = {key: value for key, value in grok_env.items() if key not in PARENT_ONLY}
    probe = str(grok_home / "sessions/.boss-preflight-write-probe")
    where = str(workspace)
    steps = [
        ("grok inspect --json", ["grok", "inspect", "--json"], grok_env,
         lambda out: '"projectTrusted": true' in out or '"projectTrusted":true' in out),
        ("grok models", ["grok", "models"], grok_env, lambda out: "You are logged in with grok.com" in out),
        ("cube workspace status", [BUNDLED_CUBE, "--json", "workspace", "status", "--workspace", where], tool_env,
         lambda out: "workspace_path" in out),
        ("gh auth status", ["gh", "auth", "status", "--active", "--hostname", "github.com"], tool_env,
         lambda out: "Logged in to github.com" in out and "(keyring)" in out),
        ("jj root", ["jj", "root"], tool_env, lambda out: out.strip() == where),
        ("jj git remote list", ["jj", "git", "remote", "list"], tool_env, lambda out: "github.com" in out),
        ("touch session probe", ["/usr/bin/touch", probe], tool_env, lambda out: True),
    ]
    records, total = [], 0.0
    for name, argv, env, affirmed in steps:
        began = time.monotonic()
        result = subprocess.run(argv, cwd=where, env=env, capture_output=True, text=True, timeout=120)
        seconds = time.monotonic() - began
        total += seconds
        records.append({
            "run": index, "step": name, "seconds": round(seconds, 4),
            "ok": result.returncode == 0 and bool(affirmed(result.stdout + "\n" + result.stderr)),
        })
    records.append({"run": index, "step": "all seven", "seconds": round(total, 4), "ok": all(r["ok"] for r in records)})
    shutil.rmtree(container, ignore_errors=True)
    return records


def brief(target_bytes):
    lines = ["Spawn-latency probe. Reply with the single word OK and then stop. "
             "Do not call any tool, do not read any file, and do not change anything.\n\n"]
    size, index = len(lines[0]), 0
    while size < target_bytes:
        lines.append(f"Padding line {index:05d}: this text only gives the brief a realistic size; ignore it.\n")
        size += len(lines[-1])
        index += 1
    return "".join(lines)


def write_private(path, text):
    with os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600), "w") as handle:
        handle.write(text)


def tui_once(index, workspace, scratch, brief_bytes, holds, max_wait):
    container = scratch / f"tui-{index:03d}-{uuid.uuid4().hex[:6]}"
    grok_home, directives, unsets = provision(container, workspace)
    hook_log = container / "hooks.log"

    def recorder(event):
        command = ("/usr/bin/perl -MTime::HiRes=time -e " + shlex.quote('printf "%s %.3f\\n", $ARGV[0], time')
                   + f" {event} >> {shlex.quote(str(hook_log))}")
        # A held hook stays open after recording, to show whether it delays the driver.
        return command + (f"; sleep {holds[event]}" if event in holds else "")

    hooks = {event: [{"matcher": "*", "hooks": [{"type": "command", "command": recorder(event), "timeout": 15}]}]
             for event in HOOK_EVENTS}
    (grok_home / "hooks/boss-provision.json").write_text(json.dumps({"hooks": hooks}))

    config_dir, prompt_home = container / "cfg", container / "prompt-home"
    config_dir.mkdir()
    prompt_home.mkdir()
    write_private(prompt_home / "initial-prompt.txt", brief(brief_bytes))
    (config_dir / "initial-prompt.txt").symlink_to(prompt_home / "initial-prompt.txt")
    write_private(config_dir / "initial-prompt.json", json.dumps(LAUNCHER_CONFIG))
    write_private(config_dir / "initial-prompt.py", LAUNCHER.read_text())
    # `build_grok_pane_command` in grok.rs.
    argv = ["grok", "--model", "grok-4.6", "--reasoning-effort", "high", "--no-alt-screen", "--always-approve",
            "--trust", "--session-id", str(uuid.uuid4()), "--cwd", str(workspace), "--no-subagents", "--no-memory"]
    script = container / "run.sh"
    script.write_text("\n".join(
        ["#!/bin/sh"] + [f"unset {key}" for key in unsets]
        + [f"export {key}={shlex.quote(value)}" for key, value in directives.items()]
        + ["exec python3 -I " + shlex.quote(str(config_dir / "initial-prompt.py")) + " " + shlex.join(argv)]) + "\n")
    script.chmod(0o700)

    server = f"spawnprobe-{uuid.uuid4().hex[:8]}"
    began_wall, began = time.time(), time.monotonic()
    subprocess.run(["tmux", "-L", server, "new-session", "-d", "-s", "probe", "-x", "220", "-y", "50",
                    "-c", str(workspace), str(script)], check=True)
    outcome = "timeout"
    try:
        while time.monotonic() - began < max_wait:
            alive = subprocess.run(["tmux", "-L", server, "has-session", "-t", "probe"], capture_output=True)
            if hook_log.exists() and "UserPromptSubmit" in hook_log.read_text():
                outcome = "prompt_accepted"
                break
            if alive.returncode != 0:
                outcome = "pane_exited"
                break
            time.sleep(0.05)
    finally:
        subprocess.run(["tmux", "-L", server, "kill-server"], capture_output=True)
        # tmux can leave the dead server's socket file behind.
        socket = Path(os.environ.get("TMUX_TMPDIR", "/tmp")) / f"tmux-{os.getuid()}" / server
        socket.unlink(missing_ok=True)

    record = {"run": index, "outcome": outcome, "brief_bytes": brief_bytes}
    delivery_log = prompt_home / "initial-prompt.delivery.jsonl"
    if delivery_log.exists():
        for line in delivery_log.read_text().splitlines():
            step = json.loads(line)
            record[step["step"]] = round(step["at_ms"] / 1000 - began_wall, 3)
    if hook_log.exists():
        for line in hook_log.read_text().splitlines():
            event, at = line.split()
            record.setdefault(event, round(float(at) - began_wall, 3))
    shutil.rmtree(container, ignore_errors=True)
    return [record]


def setup_once(index, workspace, scratch):
    container = scratch / f"setup-{index:03d}-{uuid.uuid4().hex[:6]}"
    began = time.monotonic()
    provision(container, workspace)
    home_seconds = time.monotonic() - began
    name = f"probe-{uuid.uuid4().hex[:8]}"
    began = time.monotonic()
    subprocess.run(["tmux", "-L", name, "new-session", "-d", "-s", name, "-c", str(workspace), "sleep 30"],
                   check=True, capture_output=True)
    pane_seconds = time.monotonic() - began
    subprocess.run(["tmux", "-L", name, "kill-server"], capture_output=True)
    Path(os.environ.get("TMUX_TMPDIR", "/tmp"), f"tmux-{os.getuid()}", name).unlink(missing_ok=True)
    began = time.monotonic()
    subprocess.run(["python3", "-c", "import fcntl, pty, termios"], check=True, capture_output=True)
    python_seconds = time.monotonic() - began
    began = time.monotonic()
    subprocess.run(["codex", "debug", "models", "--bundled"], check=True, capture_output=True)
    codex_seconds = time.monotonic() - began
    shutil.rmtree(container, ignore_errors=True)
    return [
        {"run": index, "step": "codex bundled model probe", "seconds": round(codex_seconds, 4)},
        {"run": index, "step": "grok home files and delegates", "seconds": round(home_seconds, 4)},
        {"run": index, "step": "tmux new-session -d", "seconds": round(pane_seconds, 4)},
        {"run": index, "step": "python3 import check", "seconds": round(python_seconds, 4)},
    ]


def summarise(title, series):
    print(f"\n{title}")
    print(f"{'':34} {'n':>3} {'min':>7} {'median':>7} {'p90':>7} {'max':>7}")
    for name, values in series.items():
        values = sorted(values)
        if not values:
            continue
        p90 = values[min(len(values) - 1, round(0.9 * (len(values) - 1)))]
        print(f"{name:34} {len(values):>3} {values[0]:>7.3f} {statistics.median(values):>7.3f} "
              f"{p90:>7.3f} {values[-1]:>7.3f}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("mode", choices=["preflight", "tui", "setup"])
    parser.add_argument("workspace", type=Path)
    parser.add_argument("scratch", type=Path)
    parser.add_argument("runs", type=int)
    parser.add_argument("--parallel", type=int, default=1, help="runs started together")
    parser.add_argument("--brief-bytes", type=int, default=40000)
    parser.add_argument("--hold", action="append", default=[], metavar="EVENT:SECONDS")
    parser.add_argument("--max-wait", type=float, default=120.0)
    args = parser.parse_args()
    workspace, scratch = args.workspace.resolve(), args.scratch.resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    holds = {event: float(seconds) for event, seconds in (spec.split(":") for spec in args.hold)}

    def once(index):
        if args.mode == "preflight":
            return preflight_once(index, workspace, scratch)
        if args.mode == "setup":
            return setup_once(index, workspace, scratch)
        return tui_once(index, workspace, scratch, args.brief_bytes, holds, args.max_wait)

    records = []
    with (scratch / f"{args.mode}.jsonl").open("a") as out:
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.parallel) as pool:
            for start in range(0, args.runs, args.parallel):
                batch = range(start, min(args.runs, start + args.parallel))
                for result in pool.map(once, batch):
                    for record in result:
                        out.write(json.dumps(record) + "\n")
                    records.extend(result)
                out.flush()

    if args.mode == "preflight":
        steps = dict.fromkeys(record["step"] for record in records)
        summarise(f"Grok spawn-preparation subprocesses, seconds ({args.parallel} at a time)",
                  {step: [r["seconds"] for r in records if r["step"] == step] for step in steps})
        print(f"\nchecks that did not pass: {sum(1 for r in records if not r['ok'])}")
    elif args.mode == "setup":
        steps = dict.fromkeys(record["step"] for record in records)
        summarise("Local setup steps, seconds",
                  {step: [r["seconds"] for r in records if r["step"] == step] for step in steps})
    else:
        accepted = [r for r in records if r["outcome"] == "prompt_accepted"]
        summarise(f"Grok pane start to first accepted prompt, seconds ({args.parallel} at a time)", {
            "SessionStart hook": [r["SessionStart"] for r in accepted if "SessionStart" in r],
            "composer ready": [r["ready"] for r in accepted if "ready" in r],
            "Return written": [r["submit_sent"] for r in accepted if "submit_sent" in r],
            "UserPromptSubmit hook": [r["UserPromptSubmit"] for r in accepted],
        })
        print(f"\nruns that did not reach an accepted prompt: {len(records) - len(accepted)}")


if __name__ == "__main__":
    main()
