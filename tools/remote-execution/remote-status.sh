#!/bin/bash
# Side-effect-free helpers for a NativeLink hub's client endpoint: parse
# grpc(s)://host:port, ask whether TCP connects, fetch /status, and say
# whether that answer means the hub is stuck.
#
# Sourced by .buildkite/steps/remote-execution.sh (CI) and
# tools/remote-execution/dev-remote-workspace-status.sh (developer Mac).
# Prints tokens on stdout; never names the endpoint on stderr (CI logs are
# public and the endpoint is host configuration).

# remote_status_endpoint_host_port <endpoint>
#
# Prints "<host> <port>" for grpc://<host>:<port> or grpcs://<host>:<port>.
# Rejects a host that contains '/' or whitespace. Returns 1 for any other
# shape, with nothing on stdout.
remote_status_endpoint_host_port() {
  local endpoint="$1" authority host port
  case "$endpoint" in
    grpc://*) authority="${endpoint#grpc://}" ;;
    grpcs://*) authority="${endpoint#grpcs://}" ;;
    *) return 1 ;;
  esac
  host="${authority%:*}"
  port="${authority##*:}"
  if [[ "$authority" != *:* || -z "$host" || ! "$port" =~ ^[0-9]+$ || "$host" == *[/[:space:]]* ]]; then
    return 1
  fi
  host="${host#\[}"
  host="${host%\]}"
  printf '%s %s\n' "$host" "$port"
}

# remote_status_tcp_reachable <os_type> <host> <port> <timeout_secs>
#
# Succeeds if a TCP connection is accepted within <timeout_secs>.
# <os_type> is `uname -s` in lower case: linux uses nc -w, everything else
# uses macOS nc's -G.
remote_status_tcp_reachable() {
  local timeout_flag="-G"
  if [[ "$1" == "linux" ]]; then
    timeout_flag="-w"
  fi
  nc -z "$timeout_flag" "$4" "$2" "$3" > /dev/null 2>&1
}

# remote_status_http <endpoint> <host> <port> <timeout_secs>
#
# GET /status over HTTP on the port Bazel uses. Prints one token: 200 for
# an executor that is answering and whose stores work, 503 for a failed or
# stalled store, `timeout` for curl exit 28 (no HTTP answer within
# <timeout_secs>), `curl:<n>` for any other curl failure, `unknown` when
# curl is missing, and any other HTTP status as itself.
remote_status_http() {
  local scheme="http" host="$2" code curl_exit=0
  if [[ "$1" == grpcs://* ]]; then
    scheme="https"
  fi
  if [[ "$host" == *:* ]]; then
    host="[${host}]"
  fi
  if ! command -v curl > /dev/null 2>&1; then
    echo unknown
    return 0
  fi
  code="$(curl --silent --output /dev/null --max-time "$4" \
    --write-out '%{http_code}' "${scheme}://${host}:$3/status" 2> /dev/null)" || curl_exit=$?
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

# remote_status_probe <endpoint> <os_type> <tcp_timeout_secs> <http_timeout_secs>
#
# TCP first, then /status. Prints `unreachable` when TCP does not connect
# (off-network, refused, connect timeout): that is not a stuck hub, and
# Bazel already falls back. Otherwise prints remote_status_http's token.
remote_status_probe() {
  local endpoint="$1" os_type="$2" tcp_timeout_s="$3" http_timeout_s="$4" host_port host port
  host_port="$(remote_status_endpoint_host_port "$endpoint")" || return 1
  host="${host_port% *}"
  port="${host_port##* }"
  if ! remote_status_tcp_reachable "$os_type" "$host" "$port" "$tcp_timeout_s"; then
    echo unreachable
    return 0
  fi
  remote_status_http "$endpoint" "$host" "$port" "$http_timeout_s"
}

# remote_status_stuck <token>
#
# Succeeds only when a hub accepted a connection and then failed to serve:
# no HTTP answer (`timeout`) or a stalled store (`503`). `unreachable` is
# not stuck: the client never reached the hub.
remote_status_stuck() {
  case "$1" in
    timeout | 503) return 0 ;;
    *) return 1 ;;
  esac
}

# How many consecutive stuck answers (`timeout` or `503`) a watchdog needs
# before it stops a build. One slow `stat` in the hub's CAS store probe (a
# hard-coded 2 s inside NativeLink) already produces a 503, and a stall on
# 2026-11-27 produced 503s over about a minute that the hub's own once-a-minute
# check never saw. At the 15 s tick, 8 consecutive bad ticks tolerates a stall
# of 105 s or more (a whole minute of bad ticks is 4 or 5) and still ends a hub
# that stays stuck in about 2 minutes (503) to 3.3 minutes (`timeout`, whose
# ticks are 15 s + the 10 s probe). tools/remote-execution/README.md, "What
# bounds a stuck action", has the derivation.
REMOTE_STATUS_STUCK_AFTER_DEFAULT=8

# remote_status_stuck_after <configured value>
#
# Prints the consecutive-bad-tick limit: the value when it is a positive
# integer, otherwise REMOTE_STATUS_STUCK_AFTER_DEFAULT.
remote_status_stuck_after() {
  if [[ "${1:-}" =~ ^[1-9][0-9]*$ ]]; then
    printf '%s\n' "$1"
  else
    printf '%s\n' "$REMOTE_STATUS_STUCK_AFTER_DEFAULT"
  fi
}

# remote_status_count_bad <count so far> <token>
#
# Prints the new run length of consecutive stuck answers: one more when
# <token> is stuck, 0 for any other answer (a good tick, `unreachable`, an
# unknown probe failure all end the run).
remote_status_count_bad() {
  if remote_status_stuck "$2"; then
    printf '%s\n' "$(($1 + 1))"
  else
    printf '%s\n' 0
  fi
}
