#!/bin/bash
#
# Tests for dev-remote-workspace-status.sh --watchdog: the developer-side
# bound on a hub that stops answering mid-build.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
watchdog="${workspace}/tools/remote-execution/dev-remote-workspace-status.sh"
tmp="${TEST_TMPDIR:?}"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

mkdir -p "${tmp}/bin"
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/bin/bash' \
  'output=/dev/null' \
  'url=""' \
  'curl_exit="${WATCHDOG_CURL_EXIT:-0}"' \
  'code="${WATCHDOG_HTTP:-200}"' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    --output) output="$2"; shift 2 ;;' \
  '    --max-time | --write-out) shift 2 ;;' \
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
export WATCHDOG_FIXTURES="${tmp}"
export PATH="${tmp}/bin:${PATH}"
: > "${tmp}/curl-calls"

run_once() {
  local status=0
  WATCHDOG_HTTP="${1}" WATCHDOG_CURL_EXIT="${2:-0}" \
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
WATCHDOG_HTTP=000 HOME="${tmp}/home" env -u DEV_REMOTE_WATCHDOG_ENDPOINT \
  "$watchdog" --watchdog --once > "${tmp}/stdout" 2> "${tmp}/stderr" || status=$?
if [[ "$status" != "1" ]]; then
  fail "~/.bazelrc endpoint: expected exit 1, got ${status}: $(cat "${tmp}/stderr")"
fi
if ! grep -q "from-rc.test:1234/status" "${tmp}/curl-calls"; then
  fail "~/.bazelrc endpoint: curl was not asked for from-rc.test: $(cat "${tmp}/curl-calls")"
fi

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
