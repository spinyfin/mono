#!/usr/bin/env python3
"""Deterministic pre-push checkleft gate (Claude Code PreToolUse hook).

Boss workers push with jj. `jj git push` is a native implementation that does
not run git's pre-push hook, so an installed git hook is inert for the worker
fleet. This hook restores the gate at the harness layer: it inspects every Bash
command and, when the command is a push (`jj git push` or `git push`), runs the
repository's checkleft against the outgoing changes before the push proceeds.
If checkleft reports errors the push is blocked and the findings are echoed
back so the worker can fix them and retry. A genuine false positive that
needs a human decision is escalated with propose blocked -- workers do not
add BYPASS_ directives themselves.

All policy lives in checkleft: zero allows the push; non-zero blocks it,
including internal errors and the pinned launcher's exit-127 failure stub.
Non-push commands, missing checkleft commands, and execution exceptions
(including timeouts) approve. The pinned launcher is not probed or replaced
when it exits non-zero: that path deliberately fails closed.

The PreToolUse payload arrives as JSON on stdin; a decision JSON is written to
stdout. Resolution order is BOSS_CHECKLEFT_BIN (used as-is), the executable
$BOSS_WORKER_BIN_DIR/checkleft launcher (unprobed), then repobin exec checkleft
using $BOSS_BIN_DIR/repobin or <repo-root>/bin/repobin, in that order, without a
repobin PATH search. The selected legacy repobin must pass an
exec checkleft --version probe; otherwise resolution falls through to
<repo-root>/bin/checkleft and finally checkleft on PATH. See
resolve_checkleft_command() for the rationale behind these fallbacks.
"""
import json
import os
import re
import shlex
import shutil
import subprocess
import sys

# The probe (`repobin exec checkleft --version`) is what pays a cold
# dispatch-cache miss: a full `bazel build //tools/checkleft:checkleft`, which
# can run into tens of minutes on a host already running several Bazel
# compiles. Budget at least two hours: a short wall-clock kill cancels the
# Bazel client, throws away the partly finished compile, and falls through
# to a stale PATH checkleft. On timeout the probe is treated as failed and
# resolution falls through to the legacy bin/checkleft / PATH lookup -- it
# does NOT fail open, since a fallback checkleft may still exist and would
# otherwise be skipped for no reason.
CHECKLEFT_PROBE_TIMEOUT_SECONDS = 7200

# Once the probe above has (if needed) warmed the dispatch cache, `checkleft
# run` can still wait on Bazel (content-hash cache miss, lock contention,
# a slow compile sharing the machine). Two hours is the floor so a still-
# compiling build is not fail-opened into an unchecked push. On timeout we
# fail open (approve) rather than strand the session -- the cube verb gates
# are the belt for that rare case. A timeout must never be treated as a
# reason to rerun the same command under the same cap.
CHECKLEFT_TIMEOUT_SECONDS = 7200

ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
DELIMS = {"&&", "||", ";", "|", "&"}


def emit(decision, reason=None):
    out = {"decision": decision}
    if reason is not None:
        out["reason"] = reason
    sys.stdout.write(json.dumps(out))
    sys.exit(0)


def command_groups(command):
    try:
        tokens = shlex.split(command, posix=True)
    except Exception:
        tokens = command.split()
    groups = []
    cur = []
    for tok in tokens:
        if tok in DELIMS:
            if cur:
                groups.append(cur)
            cur = []
        else:
            cur.append(tok)
    if cur:
        groups.append(cur)
    return groups


def is_push_command(command):
    # shlex tokenisation means a push phrase inside a quoted argument (a commit
    # message, a --body string) is a single token and never matches, so
    # `jj describe -m "git push the fix"` is correctly not treated as a push.
    for group in command_groups(command):
        i = 0
        while i < len(group) and ENV_ASSIGN_RE.match(group[i]):
            i += 1
        rest = group[i:]
        if not rest:
            continue
        prog = os.path.basename(rest[0])
        if prog == "jj":
            for j in range(1, len(rest) - 1):
                if rest[j] == "git" and rest[j + 1] == "push":
                    return True
        elif prog == "git":
            if "push" in rest[1:]:
                return True
    return False


def find_repo_root(start):
    cur = os.path.abspath(start)
    while True:
        if os.path.isdir(os.path.join(cur, ".jj")) or os.path.exists(os.path.join(cur, ".git")):
            return cur
        parent = os.path.dirname(cur)
        if parent == cur:
            return os.path.abspath(start)
        cur = parent


def resolve_repobin(root):
    # The repobin that ships with Boss comes first, by absolute path: a bare
    # PATH search finds whichever repobin the user's dotfiles put first, which
    # can be a stale or differently-pinned copy of the bundled one.
    bundle_dir = os.environ.get("BOSS_BIN_DIR", "").strip()
    if bundle_dir:
        bundled = os.path.join(bundle_dir, "repobin")
        if os.path.isfile(bundled) and os.access(bundled, os.X_OK):
            return bundled
    candidate = os.path.join(root, "bin", "repobin")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return candidate
    return None


def probe_repobin_checkleft(repobin):
    """Confirm `repobin exec checkleft` can actually dispatch before the
    gate trusts it for the real `run` invocation.

    `repobin exec checkleft` builds checkleft with `bazel build` on a
    dispatch-cache miss. If that build fails (a broken crate elsewhere in
    the tree, a bazel toolchain problem, a missing REPOBIN.toml entry),
    `repobin exec checkleft run` itself exits non-zero with nothing on
    stdout -- indistinguishable, from the gate's point of view, from a
    checkleft "internal error" -- so it would hard-block every push with no
    BYPASS_ escape, for a failure that has nothing to do with the change
    being pushed. `--version` is cheap (no CHECKS.yaml / VCS context
    needed) and shares the same on-disk dispatch cache as `run`, so probing
    with it first pays the build cost at most once and never twice.
    """
    try:
        proc = subprocess.run(
            [repobin, "exec", "checkleft", "--version"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=CHECKLEFT_PROBE_TIMEOUT_SECONDS,
        )
        return proc.returncode == 0
    except Exception:
        return False


def resolve_checkleft_command(root):
    """Return the argv prefix that runs checkleft's `run` subcommand.

    Resolution order, first match wins:

    1. `BOSS_CHECKLEFT_BIN`, an explicit override.
    2. `$BOSS_WORKER_BIN_DIR/checkleft`, the launcher the engine pinned for
       this worker. It is NOT probed: when the engine resolved no repobin the
       launcher is an exit-127 stub, so the gate fails closed (the push is
       blocked as a checkleft internal error) rather than silently running
       some other checkleft.
    3. The bundle repobin at `$BOSS_BIN_DIR/repobin`, or otherwise
       `<root>/bin/repobin` (never PATH), running `repobin exec checkleft`
       only after `probe_repobin_checkleft` confirms it can dispatch. A repobin whose
       bazel dispatch is broken falls through instead of taking the whole
       push gate down with it.
    4. `<root>/bin/checkleft`, then a `checkleft` on PATH. The PATH lookup is
       last because it can resolve an unrelated, stale build that fails the
       gate closed with "unknown implementation" errors.
    """
    override = os.environ.get("BOSS_CHECKLEFT_BIN", "").strip()
    if override:
        return [override] if os.path.exists(override) else None

    worker_bin = os.environ.get("BOSS_WORKER_BIN_DIR", "").strip()
    if worker_bin:
        pinned = os.path.join(worker_bin, "checkleft")
        if os.path.isfile(pinned) and os.access(pinned, os.X_OK):
            return [pinned]

    repobin = resolve_repobin(root)
    if repobin and probe_repobin_checkleft(repobin):
        return [repobin, "exec", "checkleft"]

    candidate = os.path.join(root, "bin", "checkleft")
    if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
        return [candidate]
    which = shutil.which("checkleft")
    return [which] if which else None


def main():
    try:
        payload = json.load(sys.stdin)
    except Exception:
        emit("approve")
    if not isinstance(payload, dict):
        emit("approve")
    if (payload.get("tool_name") or "") != "Bash":
        emit("approve")
    tool_input = payload.get("tool_input")
    if not isinstance(tool_input, dict):
        emit("approve")
    command = tool_input.get("command")
    if not isinstance(command, str) or not command.strip():
        emit("approve")
    if not is_push_command(command):
        emit("approve")

    cwd = payload.get("cwd") or os.getcwd()
    root = find_repo_root(cwd)
    checkleft_cmd = resolve_checkleft_command(root)
    if not checkleft_cmd:
        # No checkleft available -> nothing to enforce (repo may not use it).
        emit("approve")

    try:
        proc = subprocess.run(
            checkleft_cmd + ["run"],
            cwd=root,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=CHECKLEFT_TIMEOUT_SECONDS,
        )
    except Exception:
        # Could not run checkleft (timeout / exec error) -> fail open.
        emit("approve")

    if proc.returncode == 0:
        emit("approve")

    findings = (proc.stdout or "").strip()
    extra = (proc.stderr or "").strip()
    # Empty stdout with non-empty stderr means checkleft exited nonzero before
    # producing any findings -- this is an internal/operational error (e.g. a
    # VCS detection failure), not a policy violation. Use a clearly distinct
    # message so users don't try to fix policy or reach for BYPASS unnecessarily.
    if not findings:
        reason = (
            "Push blocked: checkleft internal error — this is "
            "a bug, not a policy violation. Retry after a clean "
            "`checkleft run`. If it keeps failing, use "
            "`\"$BOSS_BIN\" propose blocked --reason \"<specific question>\"` "
            "naming the error.\n\n"
            + extra
        )
    else:
        reason = (
            "Push blocked: checkleft found errors that must be fixed before "
            "pushing to GitHub.\n\n"
            + findings
            + "\n\nFix the findings above and retry the push. Do not add a "
            "`BYPASS_<CHECK_NAME>` line yourself — workers do not bypass "
            "checks. If a finding is a genuine false positive that needs a "
            "human decision, use "
            "`\"$BOSS_BIN\" propose blocked --reason \"<specific question>\"`."
        )
    emit("block", reason)


if __name__ == "__main__":
    main()
