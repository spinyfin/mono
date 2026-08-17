#!/bin/sh
# boss-remote-run — engine-owned wrapper invoked on remote hosts by the
# Boss SSH adapter to launch a worker. Owned by the engine; deployed
# via SSH/scp from `bossctl hosts add` (eager) and on dispatch when the
# embedded version drifts from what the engine expects (lazy).
#
# Contract (input via env vars):
#   BOSS_RUN_ID            — engine-assigned run id; spliced into hook events.
#   BOSS_EVENTS_SOCKET     — path to the forwarded engine events socket
#                            (typically a SSH-remote-forwarded Unix socket
#                            living under /tmp/ on this host).
#   BOSS_LEASE_ID          — cube lease id (already leased by the engine
#                            prior to invoking this wrapper; passed
#                            through to the worker process env so the
#                            shim can stamp it on every event).
#   BOSS_WORKSPACE         — absolute workspace path on this host.
#   BOSS_DRIVER            — agent-driver binary to exec (e.g. `claude`,
#                            `codex`, `grok`). Required. Comes from the
#                            execution's resolved driver — never hardcoded
#                            so a row allocated to one driver cannot
#                            silently run as another.
#   BOSS_DRIVER_COMMAND    — full shell command from that driver's SpawnPlan.
#   BOSS_DRIVER_ENV        — shell environment directives from that SpawnPlan.
#   BOSS_REPO_REMOTE_URL   — repo origin URL (used by the worker for
#                            informational logging only; cube already
#                            cloned the repo before lease was issued).
#
# Contract (output): the worker is launched DETACHED (`nohup` +
# background) so it survives the engine restarting and the launching
# SSH session closing. A detached supervisor brackets the direct driver
# child: it publishes that child's PID through `<workspace>/.boss/worker.pid`
# before the `pid=<n>` stderr handshake, and appends
# `boss-remote-run: worker exited with status=<n>` to
# `<workspace>/.boss/worker.log` after it exits. The engine reads both
# surfaces for liveness and immediate-death reporting. The wrapper's own
# exit status reports only *launch* success (0) or a sentinel
# config/toolchain/worker-PID-publication failure (78-83) — the worker's
# real lifecycle is driven by its hook events over the forwarded
# BOSS_EVENTS_SOCKET, not by this wrapper blocking. The wrapper prints
# `boss-remote-run: starting … pid=<n>` to stderr so the engine can record
# the direct worker PID in `work_runs.remote_pid`.
#
# --version: print the embedded BOSS_REMOTE_RUN_VERSION and exit 0.
# Used by the engine for the lazy version-handshake at dispatch time.

set -u

# Engine writes the canonical version string here at build time; if
# this file ships unstamped the literal sentinel below makes the drift
# obvious to a reader. Engine pushes a fresh copy on every mismatch.
BOSS_REMOTE_RUN_VERSION="__BOSS_REMOTE_RUN_VERSION__"

if [ "${1:-}" = "--version" ]; then
    printf '%s\n' "$BOSS_REMOTE_RUN_VERSION"
    exit 0
fi

# Validate the contract before doing anything destructive. Missing
# variables are an engine bug, not a user-visible failure mode, so we
# print a short diagnostic that the SSH transport will surface back to
# the engine as the wrapper exit-status reason.
required_vars="BOSS_RUN_ID BOSS_EVENTS_SOCKET BOSS_LEASE_ID BOSS_WORKSPACE BOSS_DRIVER BOSS_DRIVER_COMMAND BOSS_DRIVER_ENV"
for var in $required_vars; do
    eval "val=\${$var:-}"
    if [ -z "$val" ]; then
        printf 'boss-remote-run: required env var %s is unset\n' "$var" 1>&2
        exit 78  # EX_CONFIG: incorrect configuration
    fi
done

if [ ! -d "$BOSS_WORKSPACE" ]; then
    printf 'boss-remote-run: workspace path does not exist: %s\n' "$BOSS_WORKSPACE" 1>&2
    exit 78
fi

# Health check: the resolved driver must be reachable. The engine sets a
# documented sentinel exit code so the failure surface in
# `last_error_text` is clean (`host_missing_driver`). Name the actual
# binary so an operator never chases a misattributed `claude` install.
if ! command -v "$BOSS_DRIVER" >/dev/null 2>&1; then
    printf 'boss-remote-run: `%s` not found on PATH; install or set up the worker toolchain\n' \
        "$BOSS_DRIVER" 1>&2
    exit 79  # documented sentinel: resolved driver missing
fi

# cube must be reachable for the same reason. The engine leases the
# workspace before invoking the wrapper, but the worker may still
# invoke `cube` for status/heartbeat, so we fail-fast on missing tool.
if ! command -v cube >/dev/null 2>&1; then
    printf 'boss-remote-run: `cube` not found on PATH; install cube on this host\n' 1>&2
    exit 80  # documented sentinel: cube missing
fi

# gh must be reachable for PR creation. The engine catches expired
# tokens at heartbeat time (Phase 5) but we still need the binary present.
if ! command -v gh >/dev/null 2>&1; then
    printf 'boss-remote-run: `gh` not found on PATH; install gh on this host\n' 1>&2
    exit 81  # documented sentinel: gh missing
fi

# boss-event is the ONLY channel by which anything this worker does
# reaches the engine: the settings file wires every hook to it, and the
# engine derives activity, transcript path, completion and PR capture
# from that stream. It was previously unchecked, which made its absence
# the quietest possible failure — the worker would run to completion and
# the engine would see a run that never started, with no error anywhere.
# Preflight it exactly like the others so a host missing the shim fails
# at launch with a named reason instead of silently going dark.
if ! command -v boss-event >/dev/null 2>&1; then
    printf 'boss-remote-run: `boss-event` not found on PATH; the worker would run completely unobserved\n' 1>&2
    exit 82  # documented sentinel: boss-event missing
fi

cd "$BOSS_WORKSPACE" || {
    printf 'boss-remote-run: cd into %s failed\n' "$BOSS_WORKSPACE" 1>&2
    exit 78
}

# The shim binary on this host ships under cube's umbrella. The
# engine relies on the local cube install having put `boss-event` on
# the worker's PATH via cube's standard install. We export the env
# vars `boss-event` reads so each hook fires with the engine's
# correlation token and lease id stamped on it.
export BOSS_RUN_ID
export BOSS_EVENTS_SOCKET
export BOSS_LEASE_ID
export BOSS_WORKSPACE
export BOSS_DRIVER
export BOSS_REPO_REMOTE_URL="${BOSS_REPO_REMOTE_URL:-}"

# Per-run scratch + log dir under the cube workspace. The engine pulls
# tails of worker.log over the SSH multiplex on demand so remote runs
# get the same recent-output surface as local panes — without a second
# reverse channel. The detached supervisor appends the worker's exit
# status after the driver exits, letting the engine report an observed
# status instead of guessing why the worker disappeared.
boss_run_dir="$BOSS_WORKSPACE/.boss"
mkdir -p "$boss_run_dir" 2>/dev/null || true
worker_log="$boss_run_dir/worker.log"
worker_pid_file="$boss_run_dir/worker.pid"
rm -f "$worker_pid_file"
export BOSS_WORKER_PID_FILE="$worker_pid_file"

# Launch DETACHED so the worker survives the engine restarting and the
# launching SSH session closing: `nohup` makes the supervisor and worker
# ignore the SIGHUP the remote sshd sends on session teardown, and
# backgrounding reparents the supervisor off this wrapper. The supervisor
# writes its direct child PID before waiting for that child and
# appending its observed exit status to the same log. stdin is taken from
# /dev/null (the prompt rides the positional arg) and stdout+stderr are
# teed to the per-run log. The wrapper returns once it has the worker PID;
# the supervisor keeps running while the worker does.
#
# The command and directives originate in engine-owned driver code. Keeping
# them opaque here lets each driver own its flags, model selection, prompt
# file, and environment while this wrapper owns only remote detachment.
eval "$BOSS_DRIVER_ENV"
nohup sh -c '
    nohup sh -c "exec $BOSS_DRIVER_COMMAND" &
    worker_pid=$!
    printf "%s\\n" "$worker_pid" > "$BOSS_WORKER_PID_FILE"
    trap '\''kill "$worker_pid" 2>/dev/null || true; exit 143'\'' TERM INT HUP
    wait "$worker_pid"
    worker_status=$?
    printf "\\nboss-remote-run: worker exited with status=%s\\n" "$worker_status"
' boss-remote-worker >"$worker_log" 2>&1 </dev/null &
supervisor_pid=$!

# The supervisor starts asynchronously, so wait a bounded interval for it to
# publish the direct worker PID. Remote hosts can be busy just after an SSH
# launch, so five seconds is too short to distinguish scheduling delay from a
# broken supervisor. Do not report the supervisor PID: remote_pid drives both
# liveness reconciliation and control-channel signals.
attempt=0
while [ ! -s "$worker_pid_file" ] && [ "$attempt" -lt 300 ]; do
    sleep 0.1
    attempt=$((attempt + 1))
done
if [ ! -s "$worker_pid_file" ]; then
    printf 'boss-remote-run: worker supervisor did not publish a pid\n' 1>&2
    kill "$supervisor_pid" 2>/dev/null || true
    exit 83
fi
worker_pid="$(cat "$worker_pid_file")"
case "$worker_pid" in
    *[!0-9]*|'')
        printf 'boss-remote-run: worker supervisor published an invalid pid\n' 1>&2
        kill "$supervisor_pid" 2>/dev/null || true
        exit 83
        ;;
esac

# Echo the embedded version + worker pid so the engine sees the wrapper
# that actually ran (separate from --version, a probe-only path) and can
# record `work_runs.remote_pid`. Prefixed `boss-remote-run:` so the
# engine can recognize it amongst stderr noise without a structured
# handshake.
printf 'boss-remote-run: starting run_id=%s version=%s pid=%s driver=%s\n' \
    "$BOSS_RUN_ID" "$BOSS_REMOTE_RUN_VERSION" "$worker_pid" "$BOSS_DRIVER" 1>&2

exit 0
