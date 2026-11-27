#!/bin/bash
#
# Tests for .buildkite/steps/remote-execution.sh, the library ci-env.sh uses to
# decide whether a step's bazel actions execute on remote workers.
#
# `nc` and `curl` are stubbed with shell functions: the hermetic test sandbox
# has no network and its PATH does not carry them, and a stub is also what
# lets the test make the endpoint answer or not and see how it was probed.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../remote-execution.sh
source "${workspace}/.buildkite/steps/remote-execution.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# expect_equal <expected> <actual> <description>
expect_equal() {
  if [[ "$1" != "$2" ]]; then
    fail "$3: expected '$1', got '$2'"
  fi
}

endpoint="grpc://executor.test:50051"

# ---------------------------------------------------------------------------
# ci_remote_endpoint_host_port: splitting the endpoint for the probe.
# ---------------------------------------------------------------------------

# expect_host_port <expected> <endpoint>
expect_host_port() {
  local actual
  if ! actual="$(ci_remote_endpoint_host_port "$2" 2> /dev/null)"; then
    fail "endpoint '$2': expected '$1', call failed"
    return
  fi
  expect_equal "$1" "$actual" "endpoint '$2'"
}

expect_host_port "executor.test 50051" "$endpoint"
expect_host_port "executor.test 443" "grpcs://executor.test:443"
expect_host_port "100.64.0.7 50051" "grpc://100.64.0.7:50051"
expect_host_port "::1 50051" "grpc://[::1]:50051"

# expect_malformed <endpoint>: fails, prints nothing on stdout, and reports the
# error without repeating the value (CI logs are public; the endpoint is host
# configuration).
expect_malformed() {
  local stdout stderr_file
  stderr_file="$(mktemp "${TEST_TMPDIR:?}/stderr.XXXXXX")"
  if stdout="$(ci_remote_endpoint_host_port "$1" 2> "$stderr_file")"; then
    fail "endpoint '$1': expected failure, succeeded with '${stdout}'"
    return
  fi
  if [[ -n "$stdout" ]]; then
    fail "endpoint '$1': a failing call must print nothing on stdout, printed '${stdout}'"
  fi
  if ! grep -q "\[ci-env\] error: CI_BAZEL_REMOTE_EXECUTOR must have the form" "$stderr_file"; then
    fail "endpoint '$1': unexpected stderr: $(cat "$stderr_file")"
  fi
  if [[ -n "$1" ]] && grep -qF -- "$1" "$stderr_file"; then
    fail "endpoint '$1': the error repeats the endpoint: $(cat "$stderr_file")"
  fi
}

expect_malformed "executor.test:50051"
expect_malformed "http://executor.test:50051"
expect_malformed "grpc://executor.test"
expect_malformed "grpc://:50051"
expect_malformed "grpc://executor.test:port"
expect_malformed "grpc://executor.test:50051/instance"
expect_malformed "grpc://executor test:50051"
expect_malformed "grpc://:"

# ---------------------------------------------------------------------------
# ci_remote_execution_decide: the four conditions.
# ---------------------------------------------------------------------------

nc_calls="${TEST_TMPDIR:?}/nc-calls"
nc_status=0
nc() {
  printf '%s\n' "$*" >> "$nc_calls"
  return "$nc_status"
}

# The status request. curl_http is the HTTP status the endpoint answers with;
# 000 is what curl prints when nothing answered. A timeout is curl exit 28;
# curl_fail_exit overrides that for other failures (reset, TLS, …).
curl_calls="${TEST_TMPDIR:?}/curl-calls"
curl_http=200
curl_fail_exit=""
# curl_seq is a file of answers, one per request, consumed first; it works
# across the command substitutions the watchdog loops run probes in. Once it
# is empty, curl_http applies.
curl_seq="${TEST_TMPDIR:?}/curl-seq"
: > "$curl_seq"
curl() {
  printf '%s\n' "$*" >> "$curl_calls"
  local answer="$curl_http"
  if [[ -s "$curl_seq" ]]; then
    answer="$(head -n 1 "$curl_seq")"
    tail -n +2 "$curl_seq" > "${curl_seq}.next"
    mv "${curl_seq}.next" "$curl_seq"
  fi
  printf '%s' "$answer"
  if [[ -n "$curl_fail_exit" ]]; then
    return "$curl_fail_exit"
  fi
  if [[ "$answer" == "000" ]]; then
    return 28
  fi
  return 0
}

# decide <os_type> <allowed> <endpoint> <nc_status> [<arch>]: runs the decision
# in this shell (it reports through globals) with a clean slate and the given
# inputs. <arch> defaults to the architecture of each platform's agents.
decide() {
  local arch="${5:-}"
  if [[ -z "$arch" ]]; then
    case "$1" in
      darwin) arch=arm64 ;;
      *) arch=x86_64 ;;
    esac
  fi
  : > "$nc_calls"
  : > "$curl_calls"
  nc_status="$4"
  CI_REMOTE_EXECUTION_ALLOWED="$2"
  CI_BAZEL_REMOTE_EXECUTOR="$3"
  CI_REMOTE_EXECUTION="stale"
  CI_REMOTE_BAZEL_FLAGS="stale"
  CI_BAZEL_VERIFY_ONLY_FLAGS="stale"
  CI_REMOTE_EXECUTION_FELL_BACK="stale"
  ci_remote_execution_decide "$1" "$arch" 2> /dev/null
}

# expect_local <description> <fell_back>: off, no flags of any kind.
expect_local() {
  expect_equal "off" "$CI_REMOTE_EXECUTION" "$1: CI_REMOTE_EXECUTION"
  expect_equal "" "$CI_REMOTE_BAZEL_FLAGS" "$1: CI_REMOTE_BAZEL_FLAGS"
  expect_equal "" "$CI_BAZEL_VERIFY_ONLY_FLAGS" "$1: CI_BAZEL_VERIFY_ONLY_FLAGS"
  expect_equal "$2" "$CI_REMOTE_EXECUTION_FELL_BACK" "$1: CI_REMOTE_EXECUTION_FELL_BACK"
  if [[ -z "$CI_REMOTE_EXECUTION_REASON" ]]; then
    fail "$1: no reason given"
  fi
}

# A step that did not opt in stays on the agent even on a Mac with a working
# endpoint, and the endpoint is not even probed. This is what keeps a step that
# passes credentials to bazel off the workers until its script opts in.
decide darwin 0 "$endpoint" 0 || fail "not opted in: decide failed"
expect_local "not opted in" 0
expect_equal "" "$(cat "$nc_calls")" "not opted in: endpoint probed"
expect_equal "" "$(cat "$curl_calls")" "not opted in: status requested"

unset CI_REMOTE_EXECUTION_ALLOWED
: > "$nc_calls"
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
ci_remote_execution_decide darwin arm64 || fail "allowed unset: decide failed"
expect_local "allowed unset" 0

# Only the exact value 1 opts in.
decide darwin true "$endpoint" 0 || fail "allowed=true: decide failed"
expect_local "allowed=true" 0

# A platform with no workers builds on the agent whatever its environment
# says, and the endpoint is not probed: Linux on anything but x86-64, another
# OS, and a caller that passes no architecture.
decide linux 1 "$endpoint" 0 aarch64 || fail "linux/aarch64: decide failed"
expect_local "linux/aarch64" 0
expect_equal "" "$(cat "$nc_calls")" "linux/aarch64: endpoint probed"
decide freebsd 1 "$endpoint" 0 x86_64 || fail "freebsd: decide failed"
expect_local "freebsd" 0
: > "$nc_calls"
CI_REMOTE_EXECUTION_ALLOWED=1
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
ci_remote_execution_decide linux || fail "linux, no arch: decide failed"
expect_local "linux, no arch" 0
expect_equal "" "$(cat "$nc_calls")" "linux, no arch: endpoint probed"

# A Mac whose environment names no endpoint.
decide darwin 1 "" 0 || fail "no endpoint: decide failed"
expect_local "no endpoint" 0
unset CI_BAZEL_REMOTE_EXECUTOR
CI_REMOTE_EXECUTION_ALLOWED=1
ci_remote_execution_decide darwin arm64 || fail "endpoint unset: decide failed"
expect_local "endpoint unset" 0

# All four conditions hold.
decide darwin 1 "$endpoint" 0 || fail "remote: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "remote: CI_REMOTE_EXECUTION"
expect_equal "0" "$CI_REMOTE_EXECUTION_FELL_BACK" "remote: CI_REMOTE_EXECUTION_FELL_BACK"
expect_equal \
  "--config=ci-remote --remote_executor=${endpoint} --remote_cache=${endpoint}" \
  "$CI_REMOTE_BAZEL_FLAGS" "remote: CI_REMOTE_BAZEL_FLAGS"
expect_equal "--remote_download_minimal" "$CI_BAZEL_VERIFY_ONLY_FLAGS" "remote: CI_BAZEL_VERIFY_ONLY_FLAGS"
expect_equal "-z -G ${CI_REMOTE_PROBE_TIMEOUT_SECS} executor.test 50051" "$(cat "$nc_calls")" "remote: probe command"
expect_equal \
  "--silent --output /dev/null --max-time ${CI_REMOTE_STATUS_TIMEOUT_SECS} --write-out %{http_code} http://executor.test:50051/status" \
  "$(cat "$curl_calls")" "remote: status request"

# The endpoint does not answer: the step builds on the agent, and says that it
# was meant to execute remotely. Nothing is asked of an endpoint that did not
# even accept a connection.
decide darwin 1 "$endpoint" 1 || fail "unreachable: decide failed"
expect_local "unreachable" 1
expect_equal "" "$(cat "$curl_calls")" "unreachable: status requested"

# The endpoint accepts a connection and then says nothing (curl exit 28).
# That is a process that has stopped answering, and a bazel command sent to
# it would wait without a per-RPC deadline, so the step builds on the agent.
curl_http=000
decide darwin 1 "$endpoint" 0 || fail "silent: decide failed"
expect_local "silent" 1
if [[ "$CI_REMOTE_EXECUTION_REASON" != *"did not answer a status request"* ]]; then
  fail "silent: the reason does not say what was not answered: ${CI_REMOTE_EXECUTION_REASON}"
fi
decide linux 1 "$endpoint" 0 || fail "linux silent: decide failed"
expect_local "linux silent" 1

# The endpoint answers that a store has failed or stalled.
curl_http=503
decide darwin 1 "$endpoint" 0 || fail "unhealthy: decide failed"
expect_local "unhealthy" 1
if [[ "$CI_REMOTE_EXECUTION_REASON" != *"503"* ]]; then
  fail "unhealthy: the reason does not name the answer: ${CI_REMOTE_EXECUTION_REASON}"
fi

# An endpoint that answers but serves no status page on this port (an executor
# set up before the page was put there) is used as before: an answer of any
# other kind proves the process is serving, and nothing more can be asked.
curl_http=404
decide darwin 1 "$endpoint" 0 || fail "no status page: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "no status page: CI_REMOTE_EXECUTION"
expect_equal "0" "$CI_REMOTE_EXECUTION_FELL_BACK" "no status page: CI_REMOTE_EXECUTION_FELL_BACK"

# A probe that fails for any reason other than a timeout (reset, empty reply,
# TLS, HTTP/2-only listener) is treated the same way. Today's 1.6.6 hubs have
# no status page on the client port; only curl exit 28 means the process
# accepted a connection and then said nothing.
curl_http=000
curl_fail_exit=52
decide darwin 1 "$endpoint" 0 || fail "curl 52: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "curl 52: CI_REMOTE_EXECUTION"
expect_equal "0" "$CI_REMOTE_EXECUTION_FELL_BACK" "curl 52: CI_REMOTE_EXECUTION_FELL_BACK"
if [[ "$CI_REMOTE_EXECUTION_REASON" != *"curl exit 52"* ]]; then
  fail "curl 52: the reason does not name the curl exit: ${CI_REMOTE_EXECUTION_REASON}"
fi
curl_fail_exit=35
decide darwin 1 "$endpoint" 0 || fail "curl 35: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "curl 35: CI_REMOTE_EXECUTION"
expect_equal "0" "$CI_REMOTE_EXECUTION_FELL_BACK" "curl 35: CI_REMOTE_EXECUTION_FELL_BACK"
curl_fail_exit=""
curl_http=200

# So is any endpoint on an agent that has no curl: the connection is then all
# there is to go on, as it was before the status request existed.
command() {
  if [[ "$1" == "-v" && "$2" == "curl" ]]; then
    return 1
  fi
  builtin command "$@"
}
curl_http=000
decide darwin 1 "$endpoint" 0 || fail "no curl: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "no curl: CI_REMOTE_EXECUTION"
expect_equal "" "$(cat "$curl_calls")" "no curl: curl was run"
unset -f command
curl_http=200

# The URL of the status request: https for a TLS endpoint, and an IPv6 literal
# bracketed again (it is bare for nc).
: > "$curl_calls"
expect_equal "200" "$(ci_remote_endpoint_status "grpcs://executor.test:443" "executor.test" 443)" "status of a TLS endpoint"
expect_equal "200" "$(ci_remote_endpoint_status "grpc://[::1]:50051" "::1" 50051)" "status of an IPv6 endpoint"
expect_equal \
  "https://executor.test:443/status http://[::1]:50051/status" \
  "$(awk '{ print $NF }' "$curl_calls" | tr '\n' ' ' | sed 's/ $//')" "status request URLs"
curl_http=000
expect_equal "timeout" "$(ci_remote_endpoint_status "$endpoint" "executor.test" 50051)" "status timeout"
curl_fail_exit=56
expect_equal "curl:56" "$(ci_remote_endpoint_status "$endpoint" "executor.test" 50051)" "status curl 56"
curl_fail_exit=""
curl_http=200

# A Linux x86-64 agent: the Linux config, and the probe uses the connect
# timeout flag of the Linux nc. Everything else is the same decision.
decide linux 1 "$endpoint" 0 || fail "linux remote: decide failed"
expect_equal "on" "$CI_REMOTE_EXECUTION" "linux remote: CI_REMOTE_EXECUTION"
expect_equal "0" "$CI_REMOTE_EXECUTION_FELL_BACK" "linux remote: CI_REMOTE_EXECUTION_FELL_BACK"
expect_equal \
  "--config=ci-remote-linux --remote_executor=${endpoint} --remote_cache=${endpoint}" \
  "$CI_REMOTE_BAZEL_FLAGS" "linux remote: CI_REMOTE_BAZEL_FLAGS"
expect_equal "--remote_download_minimal" "$CI_BAZEL_VERIFY_ONLY_FLAGS" "linux remote: CI_BAZEL_VERIFY_ONLY_FLAGS"
expect_equal "-z -w ${CI_REMOTE_PROBE_TIMEOUT_SECS} executor.test 50051" "$(cat "$nc_calls")" "linux remote: probe command"
if [[ "$CI_REMOTE_EXECUTION_REASON" != *"Linux workers"* ]]; then
  fail "linux remote: the reason does not name the Linux workers: ${CI_REMOTE_EXECUTION_REASON}"
fi

decide linux 1 "$endpoint" 1 || fail "linux unreachable: decide failed"
expect_local "linux unreachable" 1

decide linux 1 "" 0 || fail "linux, no endpoint: decide failed"
expect_local "linux, no endpoint" 0

decide linux 0 "$endpoint" 0 || fail "linux, not opted in: decide failed"
expect_local "linux, not opted in" 0
expect_equal "" "$(cat "$nc_calls")" "linux, not opted in: endpoint probed"

# A malformed endpoint is an error, not a quiet local build.
if decide darwin 1 "executor.test:50051" 0; then
  fail "malformed endpoint: decide succeeded"
fi
expect_equal "off" "$CI_REMOTE_EXECUTION" "malformed endpoint: CI_REMOTE_EXECUTION"
expect_equal "" "$CI_REMOTE_BAZEL_FLAGS" "malformed endpoint: CI_REMOTE_BAZEL_FLAGS"

# ---------------------------------------------------------------------------
# ci_remote_failure_reason: telling an executor that failed from an action
# that failed. The first and the last two of the five executor failures are
# what Bazel 9.1.0 printed against NativeLink 1.8.6; the two between are
# NativeLink 1.8.6's messages as its source formats them.
# ---------------------------------------------------------------------------

# expect_failure_reason <expected substring, or "" for none> <log text>
expect_failure_reason() {
  local log reason
  log="$(mktemp "${TEST_TMPDIR:?}/bazel-log.XXXXXX")"
  printf '%s\n' "$2" > "$log"
  if reason="$(ci_remote_failure_reason "$log")"; then
    if [[ -z "$1" ]]; then
      fail "a log that shows no executor failure was given the reason '${reason}': $2"
    elif [[ "$reason" != *"$1"* ]]; then
      fail "expected a reason containing '$1', got '${reason}'"
    fi
  elif [[ -n "$1" ]]; then
    fail "no reason for a log that shows an executor failure: $2"
  fi
}

expect_failure_reason "no worker that could run an action" \
  "ERROR: BUILD.bazel:10:8: Executing genrule //:slow failed: (Exit 34): Remote Execution Failure:
Failed Precondition: Action cannot be scheduled, no worker is connected to the scheduler. No capable worker has been connected for 120s."
expect_failure_reason "lost its worker" \
  "Failed Precondition: The worker running this action was lost 7 times, more than max_worker_loss_retries (6); the action is likely what takes the worker down (OOM-killed pod, eviction). Give it a memory reservation or raise the pool's memory limit before retrying. for operation_id: 0eae26f4, maybe_worker_id: None"
expect_failure_reason "retried an action until it gave up" \
  "Failed Precondition: Job cancelled because it attempted to execute too many times 4 > 3 times for operation_id: 0eae26f4, maybe_worker_id: None"
expect_failure_reason "connection to the remote executor failed" \
  "ERROR: BUILD.bazel:10:8: Executing genrule //:slow failed: (Exit 34): UNAVAILABLE: io exception"
expect_failure_reason "did not answer when the build started" \
  "ERROR: BUILD.bazel:4:8: Executing genrule //:quick failed: Failed to query remote execution capabilities: DEADLINE_EXCEEDED: CallOptions deadline exceeded after 59.999937875s. Name resolution delay 0.000000000 seconds. [closed=[], open=[[buffered_nanos=60000624500, waiting_for_connection]]]"
# An action that ran on a worker and failed, a test that failed, and a test
# that timed out are the change's own business.
expect_failure_reason "" \
  "ERROR: BUILD.bazel:10:8: Compiling Rust rlib engine failed: (Exit 1): process_wrapper failed: error executing Rustc command"
expect_failure_reason "" "//tools/boss/engine:engine_lib_test FAILED in 12.3s"
expect_failure_reason "" "//tools/boss/engine:engine_lib_test TIMEOUT in 300.1s"
if ci_remote_failure_reason "${TEST_TMPDIR}/no-such-log" > /dev/null; then
  fail "a missing log was given a reason"
fi

# ---------------------------------------------------------------------------
# Status watchdog: the same probe as the start-of-step fallback, repeated
# while bazel runs. A silent hub (timeout) or a stalled store (503) stops
# the command; other probe failures do not.
# ---------------------------------------------------------------------------

CI_REMOTE_ENDPOINT_HOST_PORT="executor.test 50051"
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
curl_http=200
curl_fail_exit=""
: > "$curl_calls"
if ! ci_remote_watchdog_tick "$endpoint" > /dev/null; then
  fail "watchdog tick: a 200 hub was treated as stuck"
fi
curl_http=000
reason=""
if reason="$(ci_remote_watchdog_tick "$endpoint")"; then
  fail "watchdog tick: a silent hub was not treated as stuck"
fi
if [[ "$reason" != *"executor.test port 50051"* ]] || [[ "$reason" != *"did not answer a status request"* ]]; then
  fail "watchdog tick: timeout reason was '${reason}'"
fi
curl_http=503
reason=""
if reason="$(ci_remote_watchdog_tick "$endpoint")"; then
  fail "watchdog tick: a 503 hub was not treated as stuck"
fi
if [[ "$reason" != *"status 503"* ]]; then
  fail "watchdog tick: 503 reason was '${reason}'"
fi
curl_http=000
curl_fail_exit=52
if ! ci_remote_watchdog_tick "$endpoint" > /dev/null; then
  fail "watchdog tick: curl 52 was treated as stuck"
fi
curl_fail_exit=""
curl_http=404
if ! ci_remote_watchdog_tick "$endpoint" > /dev/null; then
  fail "watchdog tick: HTTP 404 was treated as stuck"
fi
curl_http=200

# A hub we cannot connect to is not stuck: Bazel falls back, and a
# --config=dev-local / off-network build must not be killed.
nc_status=1
: > "$curl_calls"
if ! ci_remote_watchdog_tick "$endpoint" > /dev/null; then
  fail "watchdog tick: an unreachable hub was treated as stuck"
fi
expect_equal "" "$(cat "$curl_calls")" "watchdog tick unreachable: curl was run"
nc_status=0

# The loop signals the bazel pid when a tick is stuck. Interval 0 so the
# test does not wait the production 15 s.
CI_REMOTE_WATCHDOG_INTERVAL_SECS=0
sleep 30 &
watch_pid=$!
reason_file="${TEST_TMPDIR:?}/watchdog-reason"
rm -f "$reason_file"
curl_http=000
ci_remote_watchdog_loop "$watch_pid" "$reason_file" "$endpoint"
wait "$watch_pid" 2>/dev/null || true
if kill -0 "$watch_pid" 2>/dev/null; then
  kill -KILL "$watch_pid" 2>/dev/null || true
  fail "watchdog loop: the process was still running after a silent hub"
fi
if [[ ! -s "$reason_file" ]] || ! grep -q "did not answer a status request" "$reason_file"; then
  fail "watchdog loop: missing reason file: $(cat "$reason_file" 2>/dev/null)"
fi
curl_http=200

# Consecutive rule: one bad tick (503 or timeout) does not stop a command.
# Only CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD in a row does, and any other answer
# resets the run.
CI_REMOTE_WATCHDOG_INTERVAL_SECS=0
CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD=3

# verdict_run <bad so far> <codes...>: one counted probe per code; prints the
# final count, and "STOP" if a reason was produced on any step.
verdict_run() {
  local bad="$1" code out stopped=""
  shift
  : > "$curl_seq"
  printf '%s\n' "$@" > "$curl_seq"
  for code in "$@"; do
    out="$(ci_remote_watchdog_verdict "$endpoint" "$bad" 2> /dev/null)"
    bad="${out%%$'\n'*}"
    [[ "$out" == *$'\n'* ]] && stopped=" STOP"
  done
  echo "${bad}${stopped}"
}
expect_equal "1" "$(verdict_run 0 503)" "consecutive: one 503"
expect_equal "2" "$(verdict_run 0 503 503)" "consecutive: N-1 503s"
expect_equal "0" "$(verdict_run 0 503 503 200)" "consecutive: 503s then 200 resets"
expect_equal "3 STOP" "$(verdict_run 0 503 503 503)" "consecutive: N 503s stop"
expect_equal "3 STOP" "$(verdict_run 0 000 000 000)" "consecutive: N timeouts stop"
expect_equal "3 STOP" "$(verdict_run 0 503 000 503)" "consecutive: mixed 503 and timeout count together"
expect_equal "1" "$(verdict_run 0 503 503 200 503)" "consecutive: the count restarts after a good tick"
# A non-stuck answer (HTTP 404) also ends the run; an unreachable hub is
# never counted at all.
expect_equal "0" "$(verdict_run 0 503 503 404)" "consecutive: a non-stuck answer resets"
nc_status=1
expect_equal "0" "$(verdict_run 0 503 503 503 503)" "consecutive: an unreachable hub never stops"
nc_status=0
# The stop reason names the run length, and every bad tick is logged.
: > "$curl_seq"
printf '%s\n' 503 > "$curl_seq"
verdict_text="$(ci_remote_watchdog_verdict "$endpoint" 2 2> "${TEST_TMPDIR}/verdict-stderr")"
if [[ "$verdict_text" != *"status 503"*"3 consecutive bad status checks"* ]]; then
  fail "consecutive: stop reason was '${verdict_text}'"
fi
if ! grep -q "bad status check (503), 3 of 3" "${TEST_TMPDIR}/verdict-stderr"; then
  fail "consecutive: bad tick was not logged: $(cat "${TEST_TMPDIR}/verdict-stderr")"
fi

# loop_runs <codes...>: run the step-level loop against a command that
# lives 1 s; prints "stopped" if the loop wrote a reason, else "kept".
loop_runs() {
  local rf="${TEST_TMPDIR}/consecutive-reason" target
  rm -f "$rf"
  printf '%s\n' "$@" > "$curl_seq"
  sleep 1 &
  target=$!
  ci_remote_watchdog_loop "$target" "$rf" "$endpoint" 2> /dev/null
  kill -KILL "$target" 2> /dev/null || true
  wait "$target" 2> /dev/null || true
  if [[ -s "$rf" ]]; then echo stopped; else echo kept; fi
}
expect_equal "kept" "$(loop_runs 503)" "loop: one 503 then 200 does not stop"
expect_equal "kept" "$(loop_runs 503 503)" "loop: N-1 503s then 200 do not stop"
expect_equal "kept" "$(loop_runs 000 000)" "loop: N-1 timeouts then 200 do not stop"
expect_equal "stopped" "$(loop_runs 503 503 503)" "loop: N consecutive 503s stop"
expect_equal "stopped" "$(loop_runs 000 000 000)" "loop: N consecutive timeouts stop"
if ! grep -q "3 consecutive bad status checks" "${TEST_TMPDIR}/consecutive-reason"; then
  fail "loop: reason line was '$(cat "${TEST_TMPDIR}/consecutive-reason")'"
fi
nc_status=1
expect_equal "kept" "$(loop_runs 503 503 503 503)" "loop: an unreachable hub never stops"
nc_status=0
: > "$curl_seq"
unset CI_REMOTE_WATCHDOG_CONSECUTIVE_BAD
expect_equal "8" "$(remote_status_stuck_after "")" "default limit"
expect_equal "8" "$(remote_status_stuck_after "0")" "default limit for 0"
expect_equal "8" "$(remote_status_stuck_after "x")" "default limit for junk"
expect_equal "5" "$(remote_status_stuck_after "5")" "configured limit"
curl_http=200

# SIGTERM diagnostic: the immediate line names the executor with no probe,
# so a silent hub cannot use up Buildkite's cancel grace period. A short
# probe then amends the reason.
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
CI_REMOTE_ENDPOINT_HOST_PORT="executor.test 50051"
: > "$curl_calls"
msg="$(ci_remote_sigterm_reason_immediate)"
if [[ "$msg" != *"SIGTERM"* ]] || [[ "$msg" != *"executor.test port 50051"* ]]; then
  fail "sigterm immediate: '${msg}'"
fi
expect_equal "" "$(cat "$curl_calls")" "sigterm immediate: curl was run"
curl_http=000
msg="$(ci_remote_sigterm_reason)"
if [[ "$msg" != *"SIGTERM"* ]] || [[ "$msg" != *"executor.test port 50051"* ]] || [[ "$msg" != *"did not answer a status request"* ]]; then
  fail "sigterm reason (timeout): '${msg}'"
fi
if [[ "$msg" != *"within ${CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS}s"* ]]; then
  fail "sigterm reason (timeout) must use the short probe budget: '${msg}'"
fi
curl_http=200
msg="$(ci_remote_sigterm_reason)"
if [[ "$msg" != *"SIGTERM"* ]] || [[ "$msg" != *"then returned 200"* ]]; then
  fail "sigterm reason (healthy): '${msg}'"
fi
unset CI_BAZEL_REMOTE_EXECUTOR
unset CI_REMOTE_ENDPOINT_HOST_PORT
msg="$(ci_remote_sigterm_reason_immediate)"
if [[ "$msg" != *"SIGTERM while remote execution was on"* ]] || [[ "$msg" == *"executor.test"* ]]; then
  fail "sigterm immediate (no endpoint): '${msg}'"
fi
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
CI_REMOTE_ENDPOINT_HOST_PORT="executor.test 50051"
curl_http=200

# Bazel clients started outside the shell wrapper (repobin, checkleft) are
# still signalled. A sibling sleep is not. `ps` is denied in the hermetic
# sandbox, so the process table is a stub the test fills in.
mkdir -p "${TEST_TMPDIR:?}/fake-bin" "${TEST_TMPDIR:?}/ps-rows"
printf '%s\n' '#!/bin/bash' 'trap "exit 0" TERM' 'while true; do sleep 1; done' \
  > "${TEST_TMPDIR}/fake-bin/bazel"
chmod +x "${TEST_TMPDIR}/fake-bin/bazel"
ps() {
  printf '%s %s %s\n' "$$" "$PPID" "$0"
  local row
  for row in "${TEST_TMPDIR}/ps-rows"/*; do
    [[ -e "$row" ]] || continue
    cat "$row"
  done
}
"${TEST_TMPDIR}/fake-bin/bazel" &
fake_bazel=$!
sleep 60 &
fake_other=$!
printf '%s %s %s\n' "$fake_bazel" "$$" "${TEST_TMPDIR}/fake-bin/bazel" \
  > "${TEST_TMPDIR}/ps-rows/bazel"
printf '%s %s %s\n' "$fake_other" "$$" "sleep 60" \
  > "${TEST_TMPDIR}/ps-rows/sleep"
ci_remote_signal_bazel_clients "$$"
wait "$fake_bazel" 2> /dev/null || true
if kill -0 "$fake_bazel" 2> /dev/null; then
  kill -KILL "$fake_bazel" 2> /dev/null || true
  kill -KILL "$fake_other" 2> /dev/null || true
  fail "signal_bazel_clients: the bazel child was still running"
fi
if ! kill -0 "$fake_other" 2> /dev/null; then
  fail "signal_bazel_clients: a non-bazel child was signalled"
fi
kill -KILL "$fake_other" 2> /dev/null || true
wait "$fake_other" 2> /dev/null || true
rm -f "${TEST_TMPDIR}/ps-rows/bazel" "${TEST_TMPDIR}/ps-rows/sleep"

# Step-level loop: a silent hub stops bazel descendants of the root pid.
CI_REMOTE_WATCHDOG_INTERVAL_SECS=0
"${TEST_TMPDIR}/fake-bin/bazel" &
tree_bazel=$!
printf '%s %s %s\n' "$tree_bazel" "$$" "${TEST_TMPDIR}/fake-bin/bazel" \
  > "${TEST_TMPDIR}/ps-rows/bazel"
reason_file="${TEST_TMPDIR:?}/watchdog-tree-reason"
rm -f "$reason_file"
curl_http=000
ci_remote_watchdog_loop_tree "$$" "$reason_file" "$endpoint"
wait "$tree_bazel" 2> /dev/null || true
if kill -0 "$tree_bazel" 2> /dev/null; then
  kill -KILL "$tree_bazel" 2> /dev/null || true
  fail "watchdog loop tree: the bazel child was still running after a silent hub"
fi
if [[ ! -s "$reason_file" ]] || ! grep -q "did not answer a status request" "$reason_file"; then
  fail "watchdog loop tree: missing reason file: $(cat "$reason_file" 2>/dev/null)"
fi
curl_http=200
rm -f "${TEST_TMPDIR}/ps-rows/bazel"
unset -f ps

# The step-level watchdog (covers bazel clients repobin/checkleft launch)
# and the SIGTERM trap live in ci-env.sh; this file holds the functions they
# call. Pin the contracts those call sites depend on.
lib="${workspace}/.buildkite/steps/remote-execution.sh"
if ! grep -q 'ci_remote_watchdog_loop_tree()' "$lib"; then
  fail "remote-execution.sh must define ci_remote_watchdog_loop_tree"
fi
if ! grep -q 'CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS="${CI_REMOTE_SIGTERM_PROBE_TIMEOUT_SECS:-3}"' "$lib"; then
  fail "SIGTERM probe budget must default to 3s so it fits in Buildkite's cancel grace period"
fi
if ! grep -q 'ci_remote_sigterm_reason_immediate' "$lib"; then
  fail "remote-execution.sh must define ci_remote_sigterm_reason_immediate"
fi
if ! grep -q 'tools/remote-execution/remote-status.sh' "$lib"; then
  fail "remote-execution.sh must source the shared remote-status helper"
fi

# ---------------------------------------------------------------------------
# The real .ci.bazelrc defines the config the flags select.
# ---------------------------------------------------------------------------

real_rc="${workspace}/.ci.bazelrc"
for line in \
  "build:ci-remote --grpc_keepalive_time=30s" \
  "build:ci-remote --grpc_keepalive_timeout=20s" \
  "build:ci-remote --noremote_local_fallback" \
  "build:ci-remote --remote_cache_compression" \
  "test:ci-remote --config=remote-tests"; do
  if ! grep -qxF -- "$line" "$real_rc"; then
    fail ".ci.bazelrc lacks the line '${line}'"
  fi
done
for line in \
  "build:ci-remote-linux --grpc_keepalive_time=30s" \
  "build:ci-remote-linux --grpc_keepalive_timeout=20s" \
  "build:ci-remote-linux --noremote_local_fallback" \
  "build:ci-remote-linux --remote_cache_compression" \
  "build:ci-remote-linux --remote_default_exec_properties=OSFamily=linux" \
  "test:ci-remote-linux --config=remote-tests"; do
  if ! grep -qxF -- "$line" "$real_rc"; then
    fail ".ci.bazelrc lacks the line '${line}'"
  fi
done
# Neither remote config names a test strategy of its own. Each selects
# `remote-tests`, whose single `remote` strategy linux_policy_source_test pins:
# a strategy line here could add a local fallback, and on Linux a test that
# fell back would run with no sandbox around it at all.
if grep -qE -- "^[a-z]+:ci-remote(-linux)? .*strategy" "$real_rc"; then
  fail ".ci.bazelrc must not name a strategy for ci-remote or ci-remote-linux"
fi
# Neither remote config may change the disk cache: a remote step has to be
# able to read what a step that built on the agent left there.
if grep -qE -- "^[a-z]+:ci-remote(-linux)? .*--disk_cache" "$real_rc"; then
  fail ".ci.bazelrc must not change --disk_cache for ci-remote or ci-remote-linux"
fi
# The endpoint is host configuration and must not be written into the repo.
if grep -qE -- "--remote_(executor|cache)=" "$real_rc"; then
  fail ".ci.bazelrc must not name a remote endpoint"
fi

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
