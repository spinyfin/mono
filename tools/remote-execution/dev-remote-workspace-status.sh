#!/bin/bash
# Workspace-status command for `dev-remote`: the original stamping script,
# plus a client-side bound on a hub that stops answering mid-build.
#
# Bazel 9.1 puts no deadline on an execution call. --grpc_keepalive_time
# (30s / 20s in .bazelrc, default 60s / 20s in 9.1) was measured not to end
# a wait against a stopped hub: the build was still waiting 17 minutes later
# with keepalive on at the default. This script is what bounds that case for
# a developer Mac. While the in-flight bazel command that spawned this
# stamping step lives, it repeats the same HTTP status request CI uses. If
# the hub accepted a connection and then does not answer (or reports a
# stalled store), it writes why to that command's terminal and stops the
# command, so bazel fails instead of waiting until the hub is restarted.
# Re-run with --config=dev-local to finish on this Mac.
#
# Usage:
#   dev-remote-workspace-status.sh            start the watchdog, then exec
#                                             tools/boss/installer/workspace-status.sh
#   dev-remote-workspace-status.sh --watchdog watchdog loop (started in the
#                                             background; tests also call this)
#   dev-remote-workspace-status.sh --watchdog --once
#                                             one probe, then exit 0 (healthy
#                                             or never connected) or 1 (stuck)
#
# Environment:
#   DEV_REMOTE_WATCHDOG_ENDPOINT   grpc://host:port or grpcs://host:port.
#                                  Unset: last build:dev-remote-endpoint
#                                  --remote_executor= line of ~/.bazelrc.
#   DEV_REMOTE_WATCHDOG_INTERVAL_SECS     default 15
#   DEV_REMOTE_WATCHDOG_STATUS_TIMEOUT_SECS  default 10
#   DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS default 5
#   DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD      consecutive bad ticks (503 or
#                                  timeout) before the build is stopped;
#                                  default 8. A good tick resets the count.
#   DEV_REMOTE_WATCHDOG_TARGET_PID  process to stop when the hub is stuck.
#                                  Unset: the bazel client connected to this
#                                  server, or the server if none is visible.
#   DEV_REMOTE_WATCHDOG=0          do not start a watchdog.
#   DEV_REMOTE_STAMP_COMMAND       override the stamping script (tests).
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=remote-status.sh
source "${here}/remote-status.sh"
original="${DEV_REMOTE_STAMP_COMMAND:-${here}/../boss/installer/workspace-status.sh}"
interval_s="${DEV_REMOTE_WATCHDOG_INTERVAL_SECS:-15}"
status_timeout_s="${DEV_REMOTE_WATCHDOG_STATUS_TIMEOUT_SECS:-10}"
connect_timeout_s="${DEV_REMOTE_WATCHDOG_CONNECT_TIMEOUT_SECS:-5}"
stuck_after="$(remote_status_stuck_after "${DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD:-}")"

watchdog_endpoint() {
  local line
  if [[ -n "${DEV_REMOTE_WATCHDOG_ENDPOINT:-}" ]]; then
    printf '%s\n' "$DEV_REMOTE_WATCHDOG_ENDPOINT"
    return 0
  fi
  if [[ -f "${HOME}/.bazelrc" ]]; then
    line="$(grep -E '^[[:space:]]*build:dev-remote-endpoint[[:space:]]+--remote_executor=' "${HOME}/.bazelrc" | tail -n 1 || true)"
    line="${line#*--remote_executor=}"
    if [[ -n "$line" ]]; then
      printf '%s\n' "$line"
      return 0
    fi
  fi
  return 1
}

watchdog_os() {
  uname -s | tr '[:upper:]' '[:lower:]'
}

watchdog_reason() {
  local host="$1" port="$2" status="$3" count_reason=""
  if [[ -n "${4:-}" ]]; then
    count_reason=" after ${4} consecutive bad status checks"
  fi
  case "$status" in
    timeout)
      echo "dev-remote watchdog: the hub at ${host} port ${port} did not answer a status request within ${status_timeout_s}s; stopping this build${count_reason}. Re-run with --config=dev-local to finish on this Mac. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
      ;;
    503)
      echo "dev-remote watchdog: the hub at ${host} port ${port} reports a failed or stalled store (status 503); stopping this build${count_reason}. Re-run with --config=dev-local to finish on this Mac. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
      ;;
    *)
      echo "dev-remote watchdog: the hub at ${host} port ${port} is stuck (status ${status}); stopping this build${count_reason}. Re-run with --config=dev-local to finish on this Mac."
      ;;
  esac
}

watchdog_reason_file() {
  printf '%s\n' "${TMPDIR:-/tmp}/dev-remote-watchdog-${1}.reason"
}

watchdog_pidfile() {
  printf '%s\n' "${TMPDIR:-/tmp}/dev-remote-watchdog-${1}.pid"
}

watchdog_log_file() {
  printf '%s\n' "${DEV_REMOTE_WATCHDOG_LOG:-${TMPDIR:-/tmp}/dev-remote-watchdog.log}"
}

# The bazel client talking to this server, if lsof can see one. The server
# is started with setsid and has no controlling terminal; the client is the
# process the developer ran and the one that should stop when the hub is
# stuck, so an idle server or a later local command is not the target.
watchdog_client_pid() {
  local server="$1" listen port client
  command -v lsof > /dev/null 2>&1 || return 1
  listen="$(lsof -nP -a -p "$server" -iTCP -sTCP:LISTEN 2> /dev/null | awk 'NR > 1 { print $9; exit }')"
  port="${listen##*:}"
  [[ "$port" =~ ^[0-9]+$ ]] || return 1
  client="$(lsof -nP -iTCP:"$port" -sTCP:ESTABLISHED 2> /dev/null | awk -v s="$server" 'NR > 1 && $2 != s { print $2; exit }')"
  [[ "$client" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$client"
}

watchdog_in_flight_pid() {
  local server="${1:-$PPID}" client
  if [[ -n "${DEV_REMOTE_WATCHDOG_TARGET_PID:-}" ]]; then
    printf '%s\n' "$DEV_REMOTE_WATCHDOG_TARGET_PID"
    return 0
  fi
  client="$(watchdog_client_pid "$server" || true)"
  if [[ "$client" =~ ^[0-9]+$ ]]; then
    printf '%s\n' "$client"
  else
    printf '%s\n' "$server"
  fi
}

watchdog_deliver_reason() {
  local target="$1" reason="$2" file tty
  file="$(watchdog_reason_file "$target")"
  printf '%s\n' "$reason" > "$file"
  printf '%s\n' "$reason" >> "$(watchdog_log_file)"
  tty="$(ps -o tty= -p "$target" 2> /dev/null | tr -d '[:space:]')"
  if [[ -n "$tty" && "$tty" != "??" && -w "/dev/${tty}" ]]; then
    printf '%s\n' "$reason" > "/dev/${tty}" 2> /dev/null || true
  fi
}

# One status probe. Prints the token (`200`, `503`, `timeout`, `unreachable`,
# ...) and succeeds, or fails with nothing printed when there is nothing to
# probe (no endpoint, malformed endpoint).
watchdog_status() {
  local endpoint host_port
  endpoint="$(watchdog_endpoint)" || return 1
  host_port="$(remote_status_endpoint_host_port "$endpoint")" || return 1
  remote_status_probe "$endpoint" "$(watchdog_os)" "$connect_timeout_s" "$status_timeout_s"
}

# The stop reason for a stuck <token>, naming the hub's host and port.
watchdog_reason_for() {
  local endpoint host_port
  endpoint="$(watchdog_endpoint)" || return 1
  host_port="$(remote_status_endpoint_host_port "$endpoint")" || return 1
  watchdog_reason "${host_port% *}" "${host_port##* }" "$1" "${2:-}"
}

# A single probe with no counting (--once). Prints the reason and returns 1
# when the answer is stuck.
watchdog_tick() {
  local status
  status="$(watchdog_status)" || return 0
  if remote_status_stuck "$status"; then
    watchdog_reason_for "$status"
    return 1
  fi
  return 0
}

watchdog_sweep_stale_pidfiles() {
  local dir="${TMPDIR:-/tmp}" f pid
  for f in "${dir}"/dev-remote-watchdog-*.pid; do
    [[ -f "$f" ]] || continue
    pid="$(cat "$f" 2> /dev/null || true)"
    if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" 2> /dev/null; then
      continue
    fi
    rm -f "$f"
  done
}

watchdog_log_bad_tick() {
  printf '%s dev-remote watchdog: bad status check (%s), %s of %s consecutive before this build is stopped\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$1" "$2" "$stuck_after" >> "$(watchdog_log_file)"
}

# Stops the target only after DEV_REMOTE_WATCHDOG_CONSECUTIVE_BAD bad ticks
# in a row. Any other answer (including `unreachable`) resets the count.
watchdog_loop() {
  local target="${DEV_REMOTE_WATCHDOG_TARGET_PID:-}" reason status bad=0
  [[ -n "$target" ]] || target="$(watchdog_in_flight_pid "$PPID")"
  _watchdog_pidfile="$(watchdog_pidfile "$target")"
  trap 'rm -f "$_watchdog_pidfile"' EXIT
  trap 'rm -f "$_watchdog_pidfile"; exit 0' TERM INT
  while kill -0 "$target" 2> /dev/null; do
    sleep "$interval_s"
    kill -0 "$target" 2> /dev/null || return 0
    status="$(watchdog_status)" || status=none
    bad="$(remote_status_count_bad "$bad" "$status")"
    if ((bad == 0)); then
      continue
    fi
    watchdog_log_bad_tick "$status" "$bad"
    if ((bad < stuck_after)); then
      continue
    fi
    reason="$(watchdog_reason_for "$status" "$bad")"
    watchdog_deliver_reason "$target" "$reason"
    kill -TERM "$target" 2> /dev/null || true
    return 0
  done
}

start_watchdog() {
  local endpoint host_port host port target pidfile log leftover
  [[ "${DEV_REMOTE_WATCHDOG:-1}" != "0" ]] || return 0
  [[ -z "${BUILDKITE_JOB_ID:-}" ]] || return 0
  endpoint="$(watchdog_endpoint)" || return 0
  host_port="$(remote_status_endpoint_host_port "$endpoint")" || return 0
  host="${host_port% *}"
  port="${host_port##* }"
  # A hub we cannot connect to is not stuck: this build will fall back
  # locally. Do not arm a watchdog that would only see `unreachable`.
  if ! remote_status_tcp_reachable "$(watchdog_os)" "$host" "$port" "$connect_timeout_s"; then
    return 0
  fi
  target="$(watchdog_in_flight_pid "$PPID")"
  leftover="$(watchdog_reason_file "$target")"
  if [[ -f "$leftover" ]]; then
    cat "$leftover" >&2
    rm -f "$leftover"
  fi
  watchdog_sweep_stale_pidfiles
  pidfile="$(watchdog_pidfile "$target")"
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile" 2> /dev/null)" 2> /dev/null; then
    return 0
  fi
  log="$(watchdog_log_file)"
  DEV_REMOTE_WATCHDOG_TARGET_PID="$target" \
    "$0" --watchdog </dev/null >/dev/null 2>>"$log" &
  echo $! > "$pidfile"
  disown $! 2> /dev/null || true
}

if [[ "${1:-}" == "--watchdog" ]]; then
  if [[ "${2:-}" == "--once" ]]; then
    if reason="$(watchdog_tick)"; then
      exit 0
    fi
    printf '%s\n' "$reason" >&2
    exit 1
  fi
  watchdog_loop
  exit 0
fi

start_watchdog
exec "$original" "$@"
