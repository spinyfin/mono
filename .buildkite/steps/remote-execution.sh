#!/bin/bash
# remote-execution.sh -- decide whether a CI step's bazel commands execute on
# remote workers or on the agent that runs the step.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects. On failure a function prints a `[ci-env] error:` line on stderr and
# returns non-zero.
#
# Why: a CI agent that builds on its own cores competes with everything else
# on that host, including the remote workers that share the hardware. Sending
# the actions to the workers leaves the agent as a thin Bazel client and lets
# the scheduler place the work.
#
# A step executes remotely only when all of these hold. Otherwise it builds on
# the agent exactly as it did before, with the disk cache from .ci.bazelrc.
#
#   1. The step opted in by setting CI_REMOTE_EXECUTION_ALLOWED=1 before
#      sourcing ci-env.sh. Opt-in, not opt-out, because a step's bazel command
#      line and action environment are sent to the workers and stored in the
#      remote cache: a release step that passes credentials through --define
#      must not start doing that because a new default appeared.
#   2. There are workers for the agent's platform: macOS, or Linux on x86-64.
#      The workers run an agent's actions with the agent's own toolchain
#      paths, so an agent only ever uses workers of its own platform. Which
#      workers those are is decided by the endpoint (condition 3): each
#      platform's agents name the endpoint whose scheduler has that
#      platform's workers.
#   3. The agent's environment names an endpoint in CI_BAZEL_REMOTE_EXECUTOR.
#      That is host configuration (the Buildkite agent's environment hook), not
#      repo configuration: every open branch carries its own copy of the repo's
#      CI files, so an endpoint written there would have to be rebased into
#      each of them when it moved, and removing the variable from an agent is
#      the way to take that agent back to local builds without a merge.
#   4. The endpoint accepts a TCP connection and, asked for its status page,
#      answers. With the scheduler down or not answering the step builds on
#      the agent rather than failing, and says so. The fallback is decided
#      once, here, for the whole step, so a step never mixes worker results
#      with agent results silently.
#
#      The second half is there because a connection proves little. The
#      kernel accepts one for a process that has stopped answering, and a
#      Bazel 9.1 client that sends its build to such a process has no
#      per-RPC deadline. Keepalive was measured not to end that wait
#      (17 minutes against a stopped hub with the 60s default). A step that
#      would have hung at its first bazel command builds on its agent
#      instead. A hub that stops answering after the step has started is
#      bounded by the status watchdog in ci-env.sh. See
#      tools/remote-execution/README.md, "What bounds a stuck action".
#
# What "remotely" covers differs by platform, and .ci.bazelrc has the config
# for each:
#
#   macOS  `ci-remote`: every action, tests included. The test wrapper's
#          Seatbelt profile is the isolation boundary and it is built on the
#          worker.
#   Linux  `ci-remote-linux`: every action, tests included. A test leaves
#          Bazel's linux-sandbox when it leaves the agent, so the isolation
#          boundary is the sandbox each Linux worker runs every action in
#          (tools/test-sandbox/linux_remote_action_sandbox.sh). The config
#          also tags every action OSFamily=linux, the property the scheduler
#          routes on: an action without it is scheduled as a macOS action.
#
# A malformed endpoint is an error, not a fallback: a typo on an agent would
# otherwise leave that agent building locally for good with nothing to show it.

# How long the TCP probe waits for the endpoint.
CI_REMOTE_PROBE_TIMEOUT_SECS=5

# How long the endpoint gets to answer the status request. A healthy one
# answers in milliseconds. One whose store has stalled gives up on its own
# check after 5 s and answers 503, so this is long enough to be told that and
# not merely time out.
CI_REMOTE_STATUS_TIMEOUT_SECS=10

# How long the SIGTERM trap's status probe may run. Buildkite's default
# cancel grace period is 10 s; the trap writes the executor-naming line
# first, then probes with this budget so SIGKILL cannot arrive first.
CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS="${CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS:-3}"

# How often a running remote bazel command re-asks the executor for its
# status. Together with CI_REMOTE_STATUS_TIMEOUT_SECS this is the bound on a
# hub that stops answering after the step has started. gRPC keepalive was
# measured not to provide that bound. Tests set this to 0.
CI_REMOTE_WATCHDOG_INTERVAL_SECS="${CI_REMOTE_WATCHDOG_INTERVAL_SECS:-15}"

# Parsing, TCP probe, /status, stuck classification: shared with the
# developer watchdog. Relative to this file so both the checkout (CI) and
# runfiles (tests) resolve it.
# shellcheck source=../../tools/remote-execution/remote-status.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../../tools/remote-execution/remote-status.sh"

_ci_remote_err() {
  echo "[ci-env] error: $*" >&2
}

_ci_remote_os() {
  printf '%s\n' "${CI_REMOTE_WATCHDOG_OS:-$(uname -s | tr '[:upper:]' '[:lower:]')}"
}

# ci_remote_endpoint_host_port <endpoint>
#
# Prints "<host> <port>" for an endpoint of the form grpc://<host>:<port> or
# grpcs://<host>:<port>. The value is not echoed in the error: it is host
# configuration and CI logs are public.
ci_remote_endpoint_host_port() {
  local out
  if ! out="$(remote_status_endpoint_host_port "$1")"; then
    _ci_remote_err "CI_BAZEL_REMOTE_EXECUTOR must have the form grpc://<host>:<port> or grpcs://<host>:<port>"
    return 1
  fi
  printf '%s\n' "$out"
}

# ci_remote_endpoint_reachable <os_type> <host> <port>
#
# Succeeds if a TCP connection to the endpoint is accepted within
# CI_REMOTE_PROBE_TIMEOUT_SECS.
ci_remote_endpoint_reachable() {
  remote_status_tcp_reachable "$1" "$2" "$3" "$CI_REMOTE_PROBE_TIMEOUT_SECS"
}

# ci_remote_endpoint_status <endpoint> <host> <port>
#
# Asks the endpoint for its status page over HTTP, on the port Bazel uses,
# and prints one token: 200 for an executor that is answering and whose
# stores work, 503 for one that reports a failed or stalled store, `timeout`
# for one that sent no answer within CI_REMOTE_STATUS_TIMEOUT_SECS (curl
# exit 28), `curl:<n>` for any other curl failure, and any other HTTP status
# for an endpoint that answers but serves no status page on this port. An
# agent without curl prints "unknown". Nothing else is echoed: the endpoint
# is host configuration.
ci_remote_endpoint_status() {
  remote_status_http "$1" "$2" "$3" "$CI_REMOTE_STATUS_TIMEOUT_SECS"
}

# ci_remote_stuck_status <status token>
#
# Succeeds for the same answers the start-of-step probe falls back on: the
# executor sent no status page within CI_REMOTE_STATUS_TIMEOUT_SECS (curl
# exit 28), or it reported a failed or stalled store (HTTP 503). Other
# tokens (another HTTP status, curl:<n>, unknown, unreachable) are not
# stuck: the process is serving, the probe could not tell, or we never
# connected.
ci_remote_stuck_status() {
  remote_status_stuck "$1"
}

# ci_remote_watchdog_reason <status token>
#
# One line naming the executor (host and port from CI_REMOTE_ENDPOINT_HOST_PORT
# when set) and what the status probe saw. For the step log and the annotation.
ci_remote_watchdog_reason() {
  local where="the remote executor"
  if [[ -n "${CI_REMOTE_ENDPOINT_HOST_PORT:-}" ]]; then
    where="the remote executor at ${CI_REMOTE_ENDPOINT_HOST_PORT% *} port ${CI_REMOTE_ENDPOINT_HOST_PORT##* }"
  fi
  case "$1" in
    timeout)
      echo "${where} accepted a connection but did not answer a status request within ${CI_REMOTE_STATUS_TIMEOUT_SECS}s while this command was running"
      ;;
    503)
      echo "${where} reports a failed or stalled store (status 503) while this command was running"
      ;;
    *)
      echo "${where} stopped answering while this command was running (status ${1})"
      ;;
  esac
}

# ci_remote_watchdog_observe <endpoint> <bad so far>
#
# One status probe, counted. Prints "<new consecutive bad count> <token>".
# The count is 0 for any answer that is not stuck, so a good tick, an
# unreachable hub or an unclassified probe failure resets the run. A single
# slow store probe in the hub answers 503, so a build is stopped only after
# CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD (default REMOTE_STATUS_STUCK_AFTER_DEFAULT)
# bad ticks in a row.
ci_remote_watchdog_observe() {
  local endpoint="$1" bad="$2" status
  ci_remote_endpoint_host_port "$endpoint" > /dev/null || { echo "0 none"; return 0; }
  status="$(remote_status_probe "$endpoint" "$(_ci_remote_os)" \
    "$CI_REMOTE_PROBE_TIMEOUT_SECS" "$CI_REMOTE_STATUS_TIMEOUT_SECS")" || status=none
  echo "$(remote_status_count_bad "$bad" "$status") ${status}"
}

# ci_remote_watchdog_verdict <endpoint> <bad so far>
#
# Runs one counted probe. Prints "<new count>" on the first line and, when
# that count reaches the limit, the stop reason on the second. Logs every bad
# tick to stderr.
ci_remote_watchdog_verdict() {
  local endpoint="$1" bad="$2" limit obs status
  limit="$(remote_status_stuck_after "${CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD:-}")"
  obs="$(ci_remote_watchdog_observe "$endpoint" "$bad")"
  bad="${obs%% *}"
  status="${obs#* }"
  echo "$bad"
  ((bad > 0)) || return 0
  echo "[ci-env] remote watchdog: bad status check (${status}), ${bad} of ${limit} consecutive before this command is stopped" >&2
  if ((bad >= limit)); then
    echo "$(ci_remote_watchdog_reason "$status"); ${bad} consecutive bad status checks"
  fi
}

# ci_remote_watchdog_loop <pid> <reason_file> <endpoint>
#
# While <pid> lives, probe the endpoint every CI_REMOTE_WATCHDOG_INTERVAL_SECS.
# After CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD stuck statuses in a row, write the
# reason to <reason_file> and send SIGTERM to <pid>. Returns once <pid> has
# exited or has been signalled.
ci_remote_watchdog_loop() {
  local pid="$1" reason_file="$2" endpoint="$3" reason verdict bad=0
  while kill -0 "$pid" 2>/dev/null; do
    sleep "$CI_REMOTE_WATCHDOG_INTERVAL_SECS"
    kill -0 "$pid" 2>/dev/null || return 0
    verdict="$(ci_remote_watchdog_verdict "$endpoint" "$bad")"
    bad="${verdict%%$'\n'*}"
    reason=""
    [[ "$verdict" == *$'\n'* ]] && reason="${verdict#*$'\n'}"
    [[ -n "$reason" ]] || continue
    printf '%s\n' "$reason" > "$reason_file"
    kill -TERM "$pid" 2>/dev/null || true
    return 0
  done
}

# ci_remote_sigterm_reason_immediate
#
# One line naming the executor, with no network. The SIGTERM trap prints
# this before it probes, so a silent hub cannot use up Buildkite's cancel
# grace period before the log names the executor.
ci_remote_sigterm_reason_immediate() {
  local endpoint="${CI_BAZEL_REMOTE_EXECUTOR:-}" host_port
  if [[ -n "${CI_REMOTE_ENDPOINT_HOST_PORT:-}" ]]; then
    echo "this step was sent SIGTERM while remote execution was on (executor at ${CI_REMOTE_ENDPOINT_HOST_PORT% *} port ${CI_REMOTE_ENDPOINT_HOST_PORT##* })"
    return 0
  fi
  if [[ -z "$endpoint" ]]; then
    echo "this step was sent SIGTERM while remote execution was on"
    return 0
  fi
  if ! host_port="$(ci_remote_endpoint_host_port "$endpoint" 2>/dev/null)"; then
    echo "this step was sent SIGTERM while remote execution was on; the configured endpoint is malformed"
    return 0
  fi
  echo "this step was sent SIGTERM while remote execution was on (executor at ${host_port% *} port ${host_port##* })"
}

# ci_remote_sigterm_reason
#
# One line for a Buildkite SIGTERM (timeout_in_minutes, cancel). Re-asks the
# executor for its status so the annotation can name what a short probe then
# saw. Uses CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS, not the 10 s start-of-step
# budget: the trap must finish before Buildkite's SIGKILL.
ci_remote_sigterm_reason() {
  local endpoint="${CI_BAZEL_REMOTE_EXECUTOR:-}" host_port status saved
  if [[ -z "$endpoint" ]]; then
    echo "this step was sent SIGTERM while remote execution was on"
    return 0
  fi
  if ! host_port="$(ci_remote_endpoint_host_port "$endpoint" 2>/dev/null)"; then
    echo "this step was sent SIGTERM while remote execution was on; the configured endpoint is malformed"
    return 0
  fi
  saved="$CI_REMOTE_STATUS_TIMEOUT_SECS"
  CI_REMOTE_STATUS_TIMEOUT_SECS="$CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS"
  status="$(ci_remote_endpoint_status "$endpoint" "${host_port% *}" "${host_port##* }")"
  CI_REMOTE_ENDPOINT_HOST_PORT="$host_port"
  if ci_remote_stuck_status "$status"; then
    echo "this step was sent SIGTERM while remote execution was on; $(ci_remote_watchdog_reason "$status")"
  else
    echo "this step was sent SIGTERM while remote execution was on; a status probe of the remote executor at ${host_port% *} port ${host_port##* } then returned ${status}"
  fi
  CI_REMOTE_STATUS_TIMEOUT_SECS="$saved"
}

# ci_remote_signal_bazel_clients <root_pid>
#
# SIGTERM every descendant of <root_pid> whose argv0 is bazel / bazelisk /
# bazel-real. That is the client process, including ones repobin and
# checkleft spawn with Command::new("bazel"). The Java server is left alone.
ci_remote_signal_bazel_clients() {
  local root="$1" pid table
  table="$(ps -o pid=,ppid=,args= 2> /dev/null)" || return 0
  printf '%s\n' "$table" | awk -v root="$root" '
    {
      pid = $1
      ppid = $2
      $1 = ""
      $2 = ""
      args = $0
      parent[pid] = ppid
      cmd[pid] = args
      pids[pid] = 1
    }
    END {
      for (pid in pids) {
        p = pid
        seen = 0
        while (p != "" && p != 0 && p != root) {
          if (walked[pid, p]++) { p = 0; break }
          p = parent[p]
        }
        if (p == root && pid != root && cmd[pid] ~ /^[[:space:]]*([^[:space:]]*\/)?(bazel|bazelisk|bazel-real)([[:space:]]|$)/) {
          print pid
        }
      }
    }
  ' | while read -r pid; do
    kill -TERM "$pid" 2> /dev/null || true
  done
}

# ci_remote_watchdog_loop_tree <root_pid> <reason_file> <endpoint>
#
# While <root_pid> lives, probe the endpoint every
# CI_REMOTE_WATCHDOG_INTERVAL_SECS. After CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD
# stuck statuses in a row, write the reason,
# call ci_remote_watchdog_stuck_hook if the caller defined it, and SIGTERM
# bazel clients under <root_pid>. Covers bazel started by the shell wrapper
# and by repobin/checkleft.
ci_remote_watchdog_loop_tree() {
  local root_pid="$1" reason_file="$2" endpoint="$3" reason verdict bad=0
  while kill -0 "$root_pid" 2> /dev/null; do
    sleep "$CI_REMOTE_WATCHDOG_INTERVAL_SECS"
    kill -0 "$root_pid" 2> /dev/null || return 0
    verdict="$(ci_remote_watchdog_verdict "$endpoint" "$bad")"
    bad="${verdict%%$'\n'*}"
    reason=""
    [[ "$verdict" == *$'\n'* ]] && reason="${verdict#*$'\n'}"
    [[ -n "$reason" ]] || continue
    printf '%s\n' "$reason" > "$reason_file"
    if declare -F ci_remote_watchdog_stuck_hook > /dev/null 2>&1; then
      ci_remote_watchdog_stuck_hook "$reason_file"
    fi
    ci_remote_signal_bazel_clients "$root_pid"
    return 0
  done
}

# ci_remote_failure_reason <bazel log file>
#
# Succeeds and prints one line if the log of a failed bazel command shows that
# the remote executor failed the build, as opposed to an action that ran and
# failed. These are the errors a build ends with when the executor cannot do
# its job, and none of them says anything about the change under test:
#
#   - the scheduler had no worker to give an action to, or none that could
#     run it (no_worker_action_timeout_s, unsatisfiable_action_timeout_s);
#   - an action lost its worker, or was retried, as often as the scheduler
#     allows (max_worker_loss_retries, max_job_retries);
#   - the connection to the executor failed and stayed failed, or the
#     executor never answered the build's first question to it.
#
# tools/remote-execution/README.md, "Timeouts", lists what produces each.
ci_remote_failure_reason() {
  local log="$1"
  if grep -q "Action cannot be scheduled" "$log" 2> /dev/null; then
    echo "the remote executor had no worker that could run an action"
  elif grep -q "The worker running this action was lost" "$log" 2> /dev/null; then
    echo "an action lost its worker as many times as the scheduler allows"
  elif grep -q "attempted to execute too many times" "$log" 2> /dev/null; then
    echo "the scheduler retried an action until it gave up"
  elif grep -q "(Exit 34): UNAVAILABLE" "$log" 2> /dev/null; then
    echo "the connection to the remote executor failed"
  elif grep -q "Failed to query remote execution capabilities" "$log" 2> /dev/null; then
    echo "the remote executor did not answer when the build started"
  else
    return 1
  fi
}

# ci_remote_execution_decide <os_type> <arch>
#
# <os_type> is `uname -s` in lower case and <arch> is `uname -m`. Applies the
# four conditions above and reports through globals rather than stdout,
# because there are several results and a command substitution would keep only
# one:
#
#   CI_REMOTE_EXECUTION            "on" or "off".
#   CI_REMOTE_EXECUTION_REASON     One line for the step's log.
#   CI_REMOTE_EXECUTION_FELL_BACK  1 when the step was meant to execute
#                                  remotely and the endpoint did not answer,
#                                  or answered that it is unhealthy.
#   CI_REMOTE_BAZEL_FLAGS          Flags to append to CI_BAZEL_FLAGS; empty
#                                  when off.
#   CI_REMOTE_ENDPOINT_HOST_PORT   "<host> <port>" of the endpoint when it was
#                                  parsed, even if the step then fell back;
#                                  empty when there is no endpoint.
#   CI_BAZEL_VERIFY_ONLY_FLAGS     Extra flags for a build or test command that
#                                  is run only for its result, so nothing reads
#                                  its outputs from disk afterwards; empty when
#                                  off. See ci-env.sh.
#
# Returns non-zero only for a malformed endpoint.
ci_remote_execution_decide() {
  local os_type="$1" arch="${2:-}" endpoint="${CI_BAZEL_REMOTE_EXECUTOR:-}" host_port config scope http_status
  CI_REMOTE_EXECUTION=off
  CI_REMOTE_EXECUTION_REASON=""
  CI_REMOTE_EXECUTION_FELL_BACK=0
  CI_REMOTE_BAZEL_FLAGS=""
  CI_REMOTE_ENDPOINT_HOST_PORT=""
  CI_BAZEL_VERIFY_ONLY_FLAGS=""

  if [[ "${CI_REMOTE_EXECUTION_ALLOWED:-0}" != "1" ]]; then
    CI_REMOTE_EXECUTION_REASON="this step has not opted in"
    return 0
  fi
  case "${os_type}/${arch}" in
    darwin/*)
      config="ci-remote"
      scope="actions execute on the remote macOS workers"
      ;;
    linux/x86_64)
      config="ci-remote-linux"
      scope="actions execute on the remote Linux workers"
      ;;
    *)
      CI_REMOTE_EXECUTION_REASON="there are no remote workers for ${os_type}/${arch:-unknown}"
      return 0
      ;;
  esac
  if [[ -z "$endpoint" ]]; then
    CI_REMOTE_EXECUTION_REASON="this agent does not set CI_BAZEL_REMOTE_EXECUTOR"
    return 0
  fi
  host_port="$(ci_remote_endpoint_host_port "$endpoint")" || return 1
  CI_REMOTE_ENDPOINT_HOST_PORT="$host_port"
  if ! ci_remote_endpoint_reachable "$os_type" "${host_port% *}" "${host_port##* }"; then
    CI_REMOTE_EXECUTION_FELL_BACK=1
    CI_REMOTE_EXECUTION_REASON="the remote executor did not accept a connection within ${CI_REMOTE_PROBE_TIMEOUT_SECS}s, so this step builds on the agent"
    return 0
  fi
  http_status="$(ci_remote_endpoint_status "$endpoint" "${host_port% *}" "${host_port##* }")"
  case "$http_status" in
    timeout)
      CI_REMOTE_EXECUTION_FELL_BACK=1
      CI_REMOTE_EXECUTION_REASON="the remote executor accepted a connection but did not answer a status request within ${CI_REMOTE_STATUS_TIMEOUT_SECS}s, so this step builds on the agent"
      return 0
      ;;
    503)
      CI_REMOTE_EXECUTION_FELL_BACK=1
      CI_REMOTE_EXECUTION_REASON="the remote executor reports a failed or stalled store (its status page answers 503), so this step builds on the agent"
      return 0
      ;;
    curl:*)
      # Connection reset, empty reply, TLS failure, HTTP/2-only listener, …
      # Today's 1.6.6 hubs have no status page on the client port; only a
      # timeout means the process accepted a connection and then said
      # nothing. Any other probe failure is treated as "no status page".
      scope="$scope (status probe failed: curl exit ${http_status#curl:}; treating the endpoint as having no status page)"
      ;;
  esac

  CI_REMOTE_EXECUTION=on
  CI_REMOTE_EXECUTION_REASON="$scope"
  CI_REMOTE_BAZEL_FLAGS="--config=${config} --remote_executor=${endpoint} --remote_cache=${endpoint}"
  CI_BAZEL_VERIFY_ONLY_FLAGS="--remote_download_minimal"
}
