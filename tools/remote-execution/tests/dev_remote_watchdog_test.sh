#!/bin/bash
#
# Tests for dev-remote-workspace-status.sh: the developer-side bound on a
# hub that stops answering mid-build.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
watchdog="${workspace}/tools/remote-execution/dev-remote-workspace-status.sh"
tmp="${TEST_TMPDIR:?}"
export TMPDIR="$tmp"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

mkdir -p "${tmp}/bin"
export WATCHDOG_FIXTURES="${tmp}"
: > "${tmp}/curl-calls"
: > "${tmp}/nc-calls"
printf '%s\n' 0 > "${tmp}/nc-status"
printf '%s\n' 200 > "${tmp}/http-code"
printf '%s\n' 0 > "${tmp}/curl-exit"

# shellcheck disable=SC2016
printf '%s\n' \
  '#!/bin/bash' \
  'output=/dev/null' \
  'url=""' \
  'curl_exit="$(cat "${WATCHDOG_FIXTURES}/curl-exit" 2>/dev/null || echo 0)"' \
  'code="$(cat "${WATCHDOG_FIXTURES}/http-code" 2>/dev/null || echo 200)"' \
  'if [[ -s "${WATCHDOG_FIXTURES}/http-seq" ]]; then' \
  '  code="$(head -n 1 "${WATCHDOG_FIXTURES}/http-seq")"' \
  '  tail -n +2 "${WATCHDOG_FIXTURES}/http-seq" > "${WATCHDOG_FIXTURES}/http-seq.next"' \
  '  mv "${WATCHDOG_FIXTURES}/http-seq.next" "${WATCHDOG_FIXTURES}/http-seq"' \
  'fi' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    --output) output="$2"; shift 2 ;;' \
  '    --max-time | --write-out | --connect-timeout) shift 2 ;;' \
  '    --silent) shift ;;' \
  '    *) url="$1"; shift ;;' \
  '  esac' \
  'done' \
  'echo "$url" >> "${WATCHDOG_FIXTURES}/curl-calls"' \
  'if [[ "$curl_exit" != "0" ]]; then' \
  '  exit "$curl_exit"' \
  'fi' \
  'printf "%s" "$code"' \
  'if [[ "$code" == "000" ]]; then' \
  '  exit 28' \
  'fi' \
  > "${tmp}/bin/curl"
chmod +x "${tmp}/bin/curl"

# shellcheck disable=SC2016
printf '%s\n' \
  '#!/bin/bash' \
  'echo "$*" >> "${WATCHDOG_FIXTURES}/nc-calls"' \
  'exit "$(cat "${WATCHDOG_FIXTURES}/nc-status" 2>/dev/null || echo 0)"' \
  > "${tmp}/bin/nc"
chmod +x "${tmp}/bin/nc"

printf '%s\n' \
  '#!/bin/bash' \
  'echo STABLE_TEST 1' \
  > "${tmp}/stamp.sh"
chmod +x "${tmp}/stamp.sh"

export PATH="${tmp}/bin:${PATH}"
export DEV_REMOTE_STAMP_COMMAND="${tmp}/stamp.sh"

run_once() {
  local status=0
  printf '%s\n' "${1}" > "${tmp}/http-code"
  printf '%s\n' "${2:-0}" > "${tmp}/curl-exit"
  printf '%s\n' 0 > "${tmp}/nc-status"
  DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
    "$watchdog" --watchdog --once > "${tmp}/stdout" 2> "${tmp}/stderr" || status=$?
  echo "$status"
}

status="$(run_once 200)"
if [[ "$status" != "0" ]]; then
  fail "healthy hub: expected exit 0, got ${status}: $(cat "${tmp}/stderr")"
fi
if [[ ! -s "${tmp}/curl-calls" ]]; then
  fail "healthy hub: curl was not invoked"
fi

status="$(run_once 000)"
if [[ "$status" != "1" ]]; then
  fail "silent hub: expected exit 1, got ${status}"
fi
if ! grep -q "hub.test port 50051" "${tmp}/stderr" || ! grep -q "did not answer a status request" "${tmp}/stderr"; then
  fail "silent hub: stderr was $(cat "${tmp}/stderr")"
fi

status="$(run_once 503)"
if [[ "$status" != "1" ]]; then
  fail "503 hub: expected exit 1, got ${status}"
fi
if ! grep -q "status 503" "${tmp}/stderr"; then
  fail "503 hub: stderr was $(cat "${tmp}/stderr")"
fi

status="$(run_once 000 52)"
if [[ "$status" != "0" ]]; then
  fail "curl 52: expected exit 0, got ${status}: $(cat "${tmp}/stderr")"
fi

# Connect timeout / unreachable: not stuck.
printf '%s\n' 1 > "${tmp}/nc-status"
printf '%s\n' 000 > "${tmp}/http-code"
: > "${tmp}/curl-calls"
status=0
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  "$watchdog" --watchdog --once > "${tmp}/stdout" 2> "${tmp}/stderr" || status=$?
if [[ "$status" != "0" ]]; then
  fail "connect timeout: expected exit 0, got ${status}: $(cat "${tmp}/stderr")"
fi
if [[ -s "${tmp}/curl-calls" ]]; then
  fail "connect timeout: curl ran despite TCP failure: $(cat "${tmp}/curl-calls")"
fi
printf '%s\n' 0 > "${tmp}/nc-status"

# No endpoint: nothing to watch, --once is healthy.
status=0
env -u DEV_REMOTE_WATCHDOG_ENDPOINT HOME="${tmp}/no-home" \
  "$watchdog" --watchdog --once > "${tmp}/stdout" 2> "${tmp}/stderr" || status=$?
if [[ "$status" != "0" ]]; then
  fail "no endpoint: expected exit 0, got ${status}: $(cat "${tmp}/stderr")"
fi

# Endpoint from ~/.bazelrc when DEV_REMOTE_WATCHDOG_ENDPOINT is unset.
mkdir -p "${tmp}/home"
printf '%s\n' 'build:dev-remote-endpoint --remote_executor=grpc://from-rc.test:1234' \
  > "${tmp}/home/.bazelrc"
: > "${tmp}/curl-calls"
status=0
printf '%s\n' 0 > "${tmp}/nc-status"
printf '%s\n' 000 > "${tmp}/http-code"
printf '%s\n' 0 > "${tmp}/curl-exit"
HOME="${tmp}/home" env -u DEV_REMOTE_WATCHDOG_ENDPOINT \
  "$watchdog" --watchdog --once > "${tmp}/stdout" 2> "${tmp}/stderr" || status=$?
if [[ "$status" != "1" ]]; then
  fail "~/.bazelrc endpoint: expected exit 1, got ${status}: $(cat "${tmp}/stderr")"
fi
if ! grep -q "from-rc.test:1234/status" "${tmp}/curl-calls"; then
  fail "~/.bazelrc endpoint: curl was not asked for from-rc.test: $(cat "${tmp}/curl-calls")"
fi

# Default mode through a command substitution must return promptly: the
# background watchdog must not keep Bazel's workspace-status stdout pipe.
printf '%s\n' 200 > "${tmp}/http-code"
printf '%s\n' 0 > "${tmp}/nc-status"
sleep 60 &
pipe_target=$!
start_s="$(date +%s)"
out="$(
  DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
    DEV_REMOTE_WATCHDOG_TARGET_PID="$pipe_target" \
    DEV_REMOTE_WATCHDOG_INTERVAL_SECS=30 \
    DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
    "$watchdog"
)"
elapsed=$(($(date +%s) - start_s))
if [[ "$out" != *STABLE_TEST* ]]; then
  fail "default mode: stamp output missing: ${out}"
fi
if ((elapsed > 5)); then
  fail "default mode: command substitution took ${elapsed}s; watchdog still holds the pipes"
fi
pidfile="${tmp}/dev-remote-watchdog-${pipe_target}.pid"
if [[ ! -f "$pidfile" ]]; then
  fail "default mode: watchdog pidfile was not created"
fi
wd_pid="$(cat "$pidfile")"
if ! kill -0 "$wd_pid" 2> /dev/null; then
  fail "default mode: watchdog exited immediately"
fi
kill -KILL "$wd_pid" "$pipe_target" 2> /dev/null || true
wait "$pipe_target" 2> /dev/null || true

# Lifecycle: stamping finishes, the target is still alive, the hub then
# goes silent. The watchdog is the same --watchdog child default mode
# starts; the test drives it in the foreground of a background job so a
# sandbox reaper cannot drop it with the stamp process.
sleep 60 &
target=$!
printf '%s\n' 0 > "${tmp}/nc-status"
printf '%s\n' 200 > "${tmp}/http-code"
stamp_out="$(
  DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
    DEV_REMOTE_WATCHDOG_TARGET_PID="$target" \
    DEV_REMOTE_WATCHDOG_INTERVAL_SECS=30 \
    DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
    "$watchdog"
)"
if [[ "$stamp_out" != *STABLE_TEST* ]]; then
  fail "lifecycle: stamp output missing: ${stamp_out}"
fi
if ! kill -0 "$target" 2> /dev/null; then
  fail "lifecycle: target died during stamping"
fi
# Stop the long-interval child from default mode, then run one with a
# 1 s interval against a silent hub.
if [[ -f "${tmp}/dev-remote-watchdog-${target}.pid" ]]; then
  kill -KILL "$(cat "${tmp}/dev-remote-watchdog-${target}.pid")" 2> /dev/null || true
fi
printf '%s\n' 000 > "${tmp}/http-code"
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  DEV_REMOTE_WATCHDOG_TARGET_PID="$target" \
  DEV_REMOTE_WATCHDOG_INTERVAL_SECS=1 \
  DEV_REMOTE_WATCHDOG_STATUS_TIMEOUT_SECS=1 \
  DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
  "$watchdog" --watchdog &
life_wd=$!
killed=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12; do
  if ! kill -0 "$target" 2> /dev/null; then
    killed=1
    break
  fi
  sleep 1
done
if [[ "$killed" != "1" ]]; then
  kill -KILL "$target" "$life_wd" 2> /dev/null || true
  fail "lifecycle: target was still running after a silent hub (log: $(cat "${tmp}/dev-remote-watchdog.log" 2>/dev/null))"
fi
reason_file="${tmp}/dev-remote-watchdog-${target}.reason"
if [[ ! -s "$reason_file" ]] || ! grep -q "did not answer a status request" "$reason_file"; then
  fail "lifecycle: missing reason file: $(cat "$reason_file" 2>/dev/null) (log: $(cat "${tmp}/dev-remote-watchdog.log" 2>/dev/null))"
fi
kill -KILL "$life_wd" 2> /dev/null || true

# Unreachable hub: default mode does not start a watchdog.
printf '%s\n' 1 > "${tmp}/nc-status"
sleep 5 &
local_target=$!
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  DEV_REMOTE_WATCHDOG_TARGET_PID="$local_target" \
  "$watchdog" > /dev/null
if [[ -f "${tmp}/dev-remote-watchdog-${local_target}.pid" ]]; then
  fail "unreachable hub: watchdog was started"
fi
kill -KILL "$local_target" 2> /dev/null || true
printf '%s\n' 0 > "${tmp}/nc-status"

# Pidfile cleanup on normal watchdog exit: when the target exits, the
# watchdog exits and removes its own pidfile.
printf '%s\n' 200 > "${tmp}/http-code"
printf '%s\n' 0 > "${tmp}/nc-status"
sleep 1 &
short_target=$!
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  DEV_REMOTE_WATCHDOG_TARGET_PID="$short_target" \
  DEV_REMOTE_WATCHDOG_INTERVAL_SECS=1 \
  DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
  "$watchdog" > /dev/null
short_pidfile="${tmp}/dev-remote-watchdog-${short_target}.pid"
if [[ ! -f "$short_pidfile" ]]; then
  fail "pidfile cleanup: pidfile was not created"
fi
wait "$short_target" 2> /dev/null || true
removed=0
for _ in 1 2 3 4 5; do
  if [[ ! -f "$short_pidfile" ]]; then
    removed=1
    break
  fi
  sleep 1
done
if [[ "$removed" != "1" ]]; then
  kill -KILL "$(cat "$short_pidfile" 2> /dev/null)" 2> /dev/null || true
  fail "pidfile cleanup: pidfile remained after target exited"
fi

# Startup sweep: removes stale pidfiles whose process is gone, keeps live ones.
true &
dead_pid=$!
wait "$dead_pid" 2> /dev/null || true
stale_pidfile="${tmp}/dev-remote-watchdog-stale-${dead_pid}.pid"
printf '%s\n' "$dead_pid" > "$stale_pidfile"

sleep 60 &
live_pid=$!
live_pidfile="${tmp}/dev-remote-watchdog-live-${live_pid}.pid"
printf '%s\n' "$live_pid" > "$live_pidfile"

sleep 60 &
sweep_target=$!
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  DEV_REMOTE_WATCHDOG_TARGET_PID="$sweep_target" \
  DEV_REMOTE_WATCHDOG_INTERVAL_SECS=30 \
  DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
  "$watchdog" > /dev/null
if [[ -f "$stale_pidfile" ]]; then
  fail "startup sweep: stale pidfile was not removed"
fi
if [[ ! -f "$live_pidfile" ]]; then
  fail "startup sweep: live pidfile was removed"
fi
sweep_pidfile="${tmp}/dev-remote-watchdog-${sweep_target}.pid"
kill -KILL "$(cat "$sweep_pidfile" 2> /dev/null)" "$live_pid" "$sweep_target" 2> /dev/null || true
wait "$live_pid" "$sweep_target" 2> /dev/null || true

# Consecutive rule: the loop stops its target only after
# DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD bad ticks (503 or timeout) in a row; a
# good tick resets the count; every bad tick is logged.
# loop_run <codes...>: answers those codes in order, then 200; prints
# "stopped" if the loop stopped its 1 s target, else "kept".
loop_run() {
  local t
  printf '%s\n' "$@" > "${tmp}/http-seq"
  printf '%s\n' 200 > "${tmp}/http-code"
  printf '%s\n' 0 > "${tmp}/nc-status"
  rm -f "${tmp}/dev-remote-watchdog-"*.reason
  : > "${tmp}/dev-remote-watchdog.log"
  sleep 2 &
  t=$!
  DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
    DEV_REMOTE_WATCHDOG_TARGET_PID="$t" \
    DEV_REMOTE_WATCHDOG_INTERVAL_SECS=0 \
    DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD=3 \
    "$watchdog" --watchdog > /dev/null 2>&1
  if [[ -s "${tmp}/dev-remote-watchdog-${t}.reason" ]]; then
    echo stopped
  else
    echo kept
  fi
  kill -KILL "$t" 2> /dev/null || true
  wait "$t" 2> /dev/null || true
  : > "${tmp}/http-seq"
}
expect() {
  if [[ "$1" != "$2" ]]; then
    fail "$3: expected $1, got $2"
  fi
}
expect kept "$(loop_run 503)" "one 503 then 200"
expect 1 "$(grep -c 'bad status check (503), 1 of 3' "${tmp}/dev-remote-watchdog.log")" "one 503 is logged with its count"
expect kept "$(loop_run 503 503)" "N-1 503s then 200"
expect kept "$(loop_run 503 503 200 503 503)" "a good tick resets the count"
expect kept "$(loop_run 000 000)" "N-1 timeouts then 200"
expect stopped "$(loop_run 503 503 503)" "N consecutive 503s"
if ! grep -q "stopping this build" "${tmp}"/dev-remote-watchdog-*.reason \
  || ! grep -q "stopping this build after 3 consecutive bad status checks\. Re-run" "${tmp}"/dev-remote-watchdog-*.reason; then
  fail "N 503s: reason line missing: $(cat "${tmp}"/dev-remote-watchdog-*.reason 2>/dev/null)"
fi
if ! grep -q "bad status check (503), 3 of 3" "${tmp}/dev-remote-watchdog.log"; then
  fail "N 503s: bad ticks not logged: $(cat "${tmp}/dev-remote-watchdog.log")"
fi
expect stopped "$(loop_run 000 000 000)" "N consecutive timeouts count like 503"
expect stopped "$(loop_run 503 000 503)" "mixed 503 and timeout count together"
# An unreachable hub is never stuck, however long it lasts.
printf '%s\n' 503 503 503 503 503 > "${tmp}/http-seq"
sleep 2 &
unreach=$!
printf '%s\n' 1 > "${tmp}/nc-status"
DEV_REMOTE_WATCHDOG_ENDPOINT="grpc://hub.test:50051" \
  DEV_REMOTE_WATCHDOG_TARGET_PID="$unreach" \
  DEV_REMOTE_WATCHDOG_INTERVAL_SECS=0 \
  DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD=3 \
  DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS=1 \
  "$watchdog" --watchdog > /dev/null 2>&1
if [[ -s "${tmp}/dev-remote-watchdog-${unreach}.reason" ]]; then
  fail "unreachable hub: the loop stopped its target"
fi
kill -KILL "$unreach" 2> /dev/null || true
wait "$unreach" 2> /dev/null || true
: > "${tmp}/http-seq"
printf '%s\n' 0 > "${tmp}/nc-status"
if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
