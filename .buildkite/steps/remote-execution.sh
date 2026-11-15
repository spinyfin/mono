#!/bin/bash
# remote-execution.sh -- decide whether a CI step's bazel commands execute on
# the remote macOS workers or on the agent that runs the step.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects. On failure a function prints a `[ci-env] error:` line on stderr and
# returns non-zero.
#
# Why: a macOS CI agent that builds and tests on its own cores competes with
# everything else on that host, including the remote workers that share the
# hardware. Sending the actions to the workers leaves the agent as a thin
# Bazel client and lets the scheduler place the work.
#
# A step executes remotely only when all of these hold. Otherwise it builds on
# the agent exactly as it did before, with the disk cache from .ci.bazelrc.
#
#   1. The step opted in by setting CI_REMOTE_EXECUTION_ALLOWED=1 before
#      sourcing ci-env.sh. Opt-in, not opt-out, because a step's bazel command
#      line and action environment are sent to the workers and stored in the
#      remote cache: a release step that passes credentials through --define
#      must not start doing that because a new default appeared.
#   2. The agent is a Mac. The workers are macOS only.
#   3. The agent's environment names an endpoint in CI_BAZEL_REMOTE_EXECUTOR.
#      That is host configuration (the Buildkite agent's environment hook), not
#      repo configuration: every open branch carries its own copy of the repo's
#      CI files, so an endpoint written there would have to be rebased into
#      each of them when it moved, and removing the variable from an agent is
#      the way to take that agent back to local builds without a merge.
#   4. The endpoint accepts a TCP connection. With the scheduler down the step
#      builds on the agent rather than failing, and says so. The fallback is
#      decided once, here, for the whole step: .bazelrc's `remote-tests`
#      config names the `remote` strategy alone on purpose, so a step never
#      mixes remote and local results silently.
#
# A malformed endpoint is an error, not a fallback: a typo on an agent would
# otherwise leave that agent building locally for good with nothing to show it.

# How long the TCP probe waits for the endpoint.
CI_REMOTE_PROBE_TIMEOUT_SECS=5

_ci_remote_err() {
  echo "[ci-env] error: $*" >&2
}

# ci_remote_endpoint_host_port <endpoint>
#
# Prints "<host> <port>" for an endpoint of the form grpc://<host>:<port> or
# grpcs://<host>:<port>. The value is not echoed in the error: it is host
# configuration and CI logs are public.
ci_remote_endpoint_host_port() {
  local endpoint="$1" authority host port
  case "$endpoint" in
    grpc://*) authority="${endpoint#grpc://}" ;;
    grpcs://*) authority="${endpoint#grpcs://}" ;;
    *)
      _ci_remote_err "CI_BAZEL_REMOTE_EXECUTOR must have the form grpc://<host>:<port> or grpcs://<host>:<port>"
      return 1
      ;;
  esac
  host="${authority%:*}"
  port="${authority##*:}"
  if [[ "$authority" != *:* || -z "$host" || ! "$port" =~ ^[0-9]+$ || "$host" == *[/[:space:]]* ]]; then
    _ci_remote_err "CI_BAZEL_REMOTE_EXECUTOR must have the form grpc://<host>:<port> or grpcs://<host>:<port>"
    return 1
  fi
  # An IPv6 literal is bracketed in the endpoint and bare for nc.
  host="${host#\[}"
  host="${host%\]}"
  printf '%s %s\n' "$host" "$port"
}

# ci_remote_endpoint_reachable <host> <port>
#
# Succeeds if a TCP connection to the endpoint is accepted within
# CI_REMOTE_PROBE_TIMEOUT_SECS. `-G` is the connect timeout of the macOS nc;
# only Mac agents reach this call.
ci_remote_endpoint_reachable() {
  nc -z -G "$CI_REMOTE_PROBE_TIMEOUT_SECS" "$1" "$2" > /dev/null 2>&1
}

# ci_remote_execution_decide <os_type>
#
# Applies the four conditions above and reports through globals rather than
# stdout, because there are several results and a command substitution would
# keep only one:
#
#   CI_REMOTE_EXECUTION            "on" or "off".
#   CI_REMOTE_EXECUTION_REASON     One line for the step's log.
#   CI_REMOTE_EXECUTION_FELL_BACK  1 when the step was meant to execute
#                                  remotely and the endpoint did not answer.
#   CI_REMOTE_BAZEL_FLAGS          Flags to append to CI_BAZEL_FLAGS; empty
#                                  when off.
#   CI_BAZEL_VERIFY_ONLY_FLAGS     Extra flags for a build or test command that
#                                  is run only for its result, so nothing reads
#                                  its outputs from disk afterwards; empty when
#                                  off. See ci-env.sh.
#
# Returns non-zero only for a malformed endpoint.
ci_remote_execution_decide() {
  local os_type="$1" endpoint="${CI_BAZEL_REMOTE_EXECUTOR:-}" host_port
  CI_REMOTE_EXECUTION=off
  CI_REMOTE_EXECUTION_REASON=""
  CI_REMOTE_EXECUTION_FELL_BACK=0
  CI_REMOTE_BAZEL_FLAGS=""
  CI_BAZEL_VERIFY_ONLY_FLAGS=""

  if [[ "${CI_REMOTE_EXECUTION_ALLOWED:-0}" != "1" ]]; then
    CI_REMOTE_EXECUTION_REASON="this step has not opted in"
    return 0
  fi
  if [[ "$os_type" != "darwin" ]]; then
    CI_REMOTE_EXECUTION_REASON="the remote workers are macOS only"
    return 0
  fi
  if [[ -z "$endpoint" ]]; then
    CI_REMOTE_EXECUTION_REASON="this agent does not set CI_BAZEL_REMOTE_EXECUTOR"
    return 0
  fi
  host_port="$(ci_remote_endpoint_host_port "$endpoint")" || return 1
  if ! ci_remote_endpoint_reachable "${host_port% *}" "${host_port##* }"; then
    CI_REMOTE_EXECUTION_FELL_BACK=1
    CI_REMOTE_EXECUTION_REASON="the remote executor did not accept a connection within ${CI_REMOTE_PROBE_TIMEOUT_SECS}s, so this step builds on the agent"
    return 0
  fi

  CI_REMOTE_EXECUTION=on
  CI_REMOTE_EXECUTION_REASON="actions execute on the remote macOS workers"
  CI_REMOTE_BAZEL_FLAGS="--config=ci-remote --remote_executor=${endpoint} --remote_cache=${endpoint}"
  CI_BAZEL_VERIFY_ONLY_FLAGS="--remote_download_minimal"
}
