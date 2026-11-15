#!/bin/bash
#
# Tests for .buildkite/steps/remote-execution.sh, the library ci-env.sh uses to
# decide whether a step's bazel actions execute on the remote macOS workers.
#
# `nc` is stubbed with a shell function: the hermetic test sandbox has no
# network and its PATH does not carry nc, and a stub is also what lets the test
# make the endpoint answer or not and see how it was probed.

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

# decide <os_type> <allowed> <endpoint> <nc_status>: runs the decision in this
# shell (it reports through globals) with a clean slate and the given inputs.
decide() {
  : > "$nc_calls"
  nc_status="$4"
  CI_REMOTE_EXECUTION_ALLOWED="$2"
  CI_BAZEL_REMOTE_EXECUTOR="$3"
  CI_REMOTE_EXECUTION="stale"
  CI_REMOTE_BAZEL_FLAGS="stale"
  CI_BAZEL_VERIFY_ONLY_FLAGS="stale"
  CI_REMOTE_EXECUTION_FELL_BACK="stale"
  ci_remote_execution_decide "$1" 2> /dev/null
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
# endpoint, and the endpoint is not even probed. This is what keeps the release
# steps, which pass credentials to bazel, off the workers.
decide darwin 0 "$endpoint" 0 || fail "not opted in: decide failed"
expect_local "not opted in" 0
expect_equal "" "$(cat "$nc_calls")" "not opted in: endpoint probed"

unset CI_REMOTE_EXECUTION_ALLOWED
: > "$nc_calls"
CI_BAZEL_REMOTE_EXECUTOR="$endpoint"
ci_remote_execution_decide darwin || fail "allowed unset: decide failed"
expect_local "allowed unset" 0

# Only the exact value 1 opts in.
decide darwin true "$endpoint" 0 || fail "allowed=true: decide failed"
expect_local "allowed=true" 0

# The workers are macOS only: a Linux agent builds locally whatever its
# environment says.
decide linux 1 "$endpoint" 0 || fail "linux: decide failed"
expect_local "linux" 0
expect_equal "" "$(cat "$nc_calls")" "linux: endpoint probed"

# A Mac whose environment names no endpoint.
decide darwin 1 "" 0 || fail "no endpoint: decide failed"
expect_local "no endpoint" 0
unset CI_BAZEL_REMOTE_EXECUTOR
CI_REMOTE_EXECUTION_ALLOWED=1
ci_remote_execution_decide darwin || fail "endpoint unset: decide failed"
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

# The endpoint does not answer: the step builds on the agent, and says that it
# was meant to execute remotely.
decide darwin 1 "$endpoint" 1 || fail "unreachable: decide failed"
expect_local "unreachable" 1

# A malformed endpoint is an error, not a quiet local build.
if decide darwin 1 "executor.test:50051" 0; then
  fail "malformed endpoint: decide succeeded"
fi
expect_equal "off" "$CI_REMOTE_EXECUTION" "malformed endpoint: CI_REMOTE_EXECUTION"
expect_equal "" "$CI_REMOTE_BAZEL_FLAGS" "malformed endpoint: CI_REMOTE_BAZEL_FLAGS"

# ---------------------------------------------------------------------------
# The real .ci.bazelrc defines the config the flags select.
# ---------------------------------------------------------------------------

real_rc="${workspace}/.ci.bazelrc"
for line in \
  "build:ci-remote --noremote_local_fallback" \
  "test:ci-remote --config=remote-tests"; do
  if ! grep -qxF -- "$line" "$real_rc"; then
    fail ".ci.bazelrc lacks the line '${line}'"
  fi
done
# The remote config must leave the disk cache alone: a remote step has to be
# able to read what a step that built on the agent left there.
if grep -qE -- "^[a-z]+:ci-remote .*--disk_cache" "$real_rc"; then
  fail ".ci.bazelrc must not change --disk_cache for ci-remote"
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
