#!/bin/bash
# Workspace-status command for `dev-remote`: the original stamping script,
# plus a client-side bound on a hub that stops answering mid-build.
#
# Bazel 9.1 puts no deadline on an execution call. --grpc_keepalive_time
# (30s / 20s in .bazelrc, default 60s / 20s in 9.1) was measured not to end
# a wait against a stopped hub: the build was still waiting 17 minutes later
# with keepalive on at the default. This script is what bounds that case for
# a developer Mac. While the Bazel server that spawned it lives, it repeats
# the same HTTP status request CI uses. If the hub does not answer (curl exit
# 28) or reports a stalled store (503), it prints why on the terminal and
# stops the server, so the bazel command fails instead of waiting until the
# hub is restarted. Re-run with --config=dev-local to finish on this Mac.
#
# Usage:
#   dev-remote-workspace-status.sh            start the watchdog, then exec
#                                             tools/boss/installer/workspace-status.sh
#   dev-remote-workspace-status.sh --watchdog watchdog loop (started in the
#                                             background; tests also call this)
#   dev-remote-workspace-status.sh --watchdog --once
#                                             one probe, then exit 0 (healthy)
#                                             or 1 (stuck)
#
# Environment:
#   DEV_REMOTE_WATCHDOG_ENDPOINT   grpc://host:port or grpcs://host:port.
#                                  Unset: last build:dev-remote-endpoint
#                                  --remote_executor= line of ~/.bazelrc.
#   DEV_REMOTE_WATCHDOG_INTERVAL_SECS     default 15
#   DEV_REMOTE_WATCHDOG_STATUS_TIMEOUT_SECS  default 10
#   DEV_REMOTE_WATCHDOG_TARGET_PID  process to stop when the hub is stuck.
#                                  Unset: the parent (the Bazel server).
#   DEV_REMOTE_WATCHDOG=0          do not start a watchdog.
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
original="${here}/../boss/installer/workspace-status.sh"
interval_s="${DEV_REMOTE_WATCHDOG_INTERVAL_SECS:-15}"
status_timeout_s="${DEV_REMOTE_WATCHDOG_STATUS_TIMEOUT_SECS:-10}"

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

watchdog_host_port() {
  local endpoint="$1" authority host port
  case "$endpoint" in
    grpc://*) authority="${endpoint#grpc://}" ;;
    grpcs://*) authority="${endpoint#grpcs://}" ;;
    *) return 1 ;;
  esac
  host="${authority%:*}"
  port="${authority##*:}"
  if [[ "$authority" != *:* || -z "$host" || ! "$port" =~ ^[0-9]+$ ]]; then
    return 1
  fi
  host="${host#\[}"
  host="${host%\]}"
  printf '%s %s\n' "$host" "$port"
}

watchdog_status() {
  local endpoint="$1" host="$2" port="$3" scheme="http" code curl_exit=0
  if [[ "$endpoint" == grpcs://* ]]; then
    scheme="https"
  fi
  if [[ "$host" == *:* ]]; then
    host="[${host}]"
  fi
  if ! command -v curl > /dev/null 2>&1; then
    echo unknown
    return 0
  fi
  code="$(curl --silent --output /dev/null --max-time "$status_timeout_s" \
    --write-out '%{http_code}' "${scheme}://${host}:${port}/status" 2> /dev/null)" || curl_exit=$?
  if ((curl_exit == 28)); then
    echo timeout
    return 0
  fi
  if ((curl_exit != 0)); then
    echo "curl:${curl_exit}"
    return 0
  fi
  echo "${code:-000}"
}

watchdog_reason() {
  local host="$1" port="$2" status="$3"
  case "$status" in
    timeout)
      echo "dev-remote watchdog: the hub at ${host} port ${port} did not answer a status request within ${status_timeout_s}s; stopping this build. Re-run with --config=dev-local to finish on this Mac. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
      ;;
    503)
      echo "dev-remote watchdog: the hub at ${host} port ${port} reports a failed or stalled store (status 503); stopping this build. Re-run with --config=dev-local to finish on this Mac. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
      ;;
    *)
      echo "dev-remote watchdog: the hub at ${host} port ${port} is stuck (status ${status}); stopping this build. Re-run with --config=dev-local to finish on this Mac."
      ;;
  esac
}

watchdog_tick() {
  local endpoint host_port host port status
  endpoint="$(watchdog_endpoint)" || return 0
  host_port="$(watchdog_host_port "$endpoint")" || return 0
  host="${host_port% *}"
  port="${host_port##* }"
  status="$(watchdog_status "$endpoint" "$host" "$port")"
  case "$status" in
    timeout | 503)
      watchdog_reason "$host" "$port" "$status"
      return 1
      ;;
  esac
  return 0
}

watchdog_loop() {
  local target="${DEV_REMOTE_WATCHDOG_TARGET_PID:-$PPID}" reason
  while kill -0 "$target" 2>/dev/null; do
    sleep "$interval_s"
    kill -0 "$target" 2>/dev/null || return 0
    if reason="$(watchdog_tick)"; then
      continue
    fi
    if [[ -w /dev/tty ]]; then
      printf '%s\n' "$reason" > /dev/tty
    else
      printf '%s\n' "$reason" >&2
    fi
    kill -TERM "$target" 2>/dev/null || true
    return 0
  done
}

start_watchdog() {
  local endpoint pidfile
  [[ "${DEV_REMOTE_WATCHDOG:-1}" != "0" ]] || return 0
  [[ -z "${BUILDKITE_JOB_ID:-}" ]] || return 0
  endpoint="$(watchdog_endpoint)" || return 0
  watchdog_host_port "$endpoint" > /dev/null || return 0
  pidfile="${TMPDIR:-/tmp}/dev-remote-watchdog-${PPID}.pid"
  if [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile" 2> /dev/null)" 2> /dev/null; then
    return 0
  fi
  "$0" --watchdog &
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
