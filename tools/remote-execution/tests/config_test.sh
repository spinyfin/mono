#!/bin/bash
#
# Pins what the remote-execution files promise each other.
#
# None of these files is exercised by a build: they are installed on the hub
# and the workers by hand. A value changed in one of them and not in the file
# that depends on it still looks fine in review and fails on the host, or
# worse, does not fail: a scheduler ceiling below what a worker may spend on
# one action ends work that would have finished.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
dir="${workspace}/tools/remote-execution"
hub="${dir}/nativelink/hub.json5"
worker="${dir}/nativelink/worker-macos.json5"
plist="${dir}/nativelink/com.brianduff.nativelink.plist"
hub_unit="${dir}/nativelink/nativelink-macos-hub.service"
health_unit="${dir}/nativelink/nativelink-macos-hub-health.service"
health_timer="${dir}/nativelink/nativelink-macos-hub-health.timer"
health="${dir}/nativelink/hub-health.sh"
build="${dir}/nativelink/build-worker.sh"
patch="${dir}/nativelink/patches/macos-clonefile-executables.patch"
readme="${dir}/README.md"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# Prints the lines of a JSON5 file that are not comments, without indentation.
settings() {
  grep -v -E '^[[:space:]]*//' "$1" | sed -E 's/^[[:space:]]+//'
}

# setting <file> <key>: the number a `key: <number>,` line of the file sets.
# Fails the test if the file does not set it exactly once.
setting() {
  local values
  values="$(settings "$1" | sed -n -E "s/^$2: ([0-9]+),\$/\1/p")"
  if [[ ! "$values" =~ ^[0-9]+$ ]]; then
    fail "$(basename "$1") must set $2 exactly once, to a number (found: '${values}')"
    echo 0
    return
  fi
  echo "$values"
}

# ---------------------------------------------------------------------------
# The scheduler's timeouts: all written out, and each the value the runbook's
# table gives a reason for.
# ---------------------------------------------------------------------------

expect_setting() {
  local actual
  actual="$(setting "$1" "$2")"
  if [[ "$actual" != "$3" ]]; then
    fail "$(basename "$1"): $2 is ${actual}, expected $3. If the change is meant, change the reason next to it, README.md's table and this test together."
  fi
  # The runbook's table has a row for it with the same value.
  if ! grep -q -E "^\| \`$2\` +\| $3 " "$readme"; then
    fail "README.md's \"Timeouts\" table has no row giving $2 as $3"
  fi
}

expect_setting "$hub" worker_timeout_s 30
expect_setting "$hub" max_action_executing_timeout_s 2700
expect_setting "$hub" executing_sweep_interval_s 30
expect_setting "$hub" unacknowledged_kill_timeout_s 60
expect_setting "$hub" dispatch_ack_timeout_s 20
expect_setting "$hub" client_action_timeout_s 60
expect_setting "$hub" max_job_retries 3
expect_setting "$hub" max_worker_loss_retries 6
expect_setting "$hub" max_timeout_retries 1
expect_setting "$hub" no_worker_action_timeout_s 120
expect_setting "$hub" unsatisfiable_action_timeout_s 300
expect_setting "$hub" execute_stream_heartbeat_s 30
expect_setting "$worker" max_download_timeout_s 600
expect_setting "$worker" max_action_timeout_s 1200
expect_setting "$worker" max_upload_timeout_s 600
expect_setting "$worker" http2_keepalive_interval_s 10
expect_setting "$worker" http2_keepalive_timeout_s 20

# The scheduler's ceiling on an executing action is above everything a healthy
# worker may spend on one, so that the worker's own error is the one reported.
# Below the sum it ends work that would have finished.
worker_budget=$(($(setting "$worker" max_download_timeout_s) + $(setting "$worker" max_action_timeout_s) + $(setting "$worker" max_upload_timeout_s)))
ceiling="$(setting "$hub" max_action_executing_timeout_s)"
if ((ceiling <= worker_budget)); then
  fail "hub.json5's max_action_executing_timeout_s (${ceiling}) must exceed the worker's fetch + command + upload limits (${worker_budget})"
fi

# A worker notices a connection that has stopped answering no later than the
# scheduler evicts a worker it has not heard from. Otherwise it goes on running
# actions for a scheduler that has already given them to another worker.
notice=$(($(setting "$worker" http2_keepalive_interval_s) + $(setting "$worker" http2_keepalive_timeout_s)))
evict="$(setting "$hub" worker_timeout_s)"
if ((notice > evict)); then
  fail "the worker notices a dead connection after ${notice}s, later than the hub's worker_timeout_s (${evict}s)"
fi

# The scheduler waits for several keepalives before it calls a worker silent.
# The worker sends one every half of worker_api_endpoint.timeout.
keepalive_timeout="$(settings "$worker" | sed -n -E 's/^timeout: ([0-9]+),$/\1/p')"
if [[ ! "$keepalive_timeout" =~ ^[0-9]+$ ]]; then
  fail "worker-macos.json5 must set worker_api_endpoint.timeout to a whole number"
elif ((evict < 4 * keepalive_timeout)); then
  fail "worker_timeout_s (${evict}s) is under eight keepalive intervals (timeout ${keepalive_timeout}s, a keepalive every half of it)"
fi

# ---------------------------------------------------------------------------
# Every variable a config expands is set by the unit that starts it.
# ---------------------------------------------------------------------------

variables() {
  settings "$1" | grep -o -E '\$\{[A-Z_]+\}' | tr -d '${}' | sort -u
}

for name in $(variables "$worker"); do
  if ! grep -q -F "<key>${name}</key>" "$plist"; then
    fail "worker-macos.json5 expands \${${name}}, which the launchd plist does not set"
  fi
  if ! grep -q -E "^//   ${name} " "$worker"; then
    fail "worker-macos.json5 expands \${${name}} without describing it in its header"
  fi
done
for name in $(variables "$hub"); do
  # The hub's come from hub.env, which is host configuration and not in the
  # repository, so the header comment is the only place they are listed.
  if ! grep -q -E "^//   ${name} " "$hub"; then
    fail "hub.json5 expands \${${name}} without describing it in its header"
  fi
  if ! grep -q -F "${name}=" "$readme"; then
    fail "README.md's hub.env example does not set ${name}"
  fi
done
grep -q -x 'EnvironmentFile=%h/nativelink-macos-hub/hub.env' "$hub_unit" ||
  fail "the hub's unit must read hub.env"
grep -q -x 'EnvironmentFile=%h/nativelink-macos-hub/hub.env' "$health_unit" ||
  fail "the health check's unit must read hub.env, where the admin address is"

# ---------------------------------------------------------------------------
# The health check reads what the hub is configured to provide.
# ---------------------------------------------------------------------------

# JSON log lines, which is the only format the check counts events in.
grep -q -x 'Environment=NL_LOG=json' "$hub_unit" ||
  fail "the hub's unit must set NL_LOG=json: hub-health.sh reads that format"
# The journal of the unit the hub runs as.
grep -q -x 'ExecStart=%h/nativelink-macos-hub/hub-health.sh --journal-unit nativelink-macos-hub.service' "$health_unit" ||
  fail "the health check must read the journal of nativelink-macos-hub.service"
grep -q -x 'OnUnitActiveSec=1min' "$health_timer" ||
  fail "the health check must run every minute"
# The scheduler by the name the admin API knows it under.
scheduler="$(sed -n -E 's/^SCHEDULER_NAME="([A-Z_]+)"$/\1/p' "$health")"
if [[ -z "$scheduler" ]] || ! settings "$hub" | grep -q -x "name: \"${scheduler}\","; then
  fail "hub-health.sh asks the admin API about scheduler '${scheduler}', which hub.json5 does not define"
fi
# The admin API and a status page on the loopback listener, and the status
# page on the client port too, for a CI step's probe.
settings "$hub" | grep -q -x 'services: { admin: {}, health: {} },' ||
  fail "hub.json5 must serve the admin API and a status page on the admin listener"
if [[ "$(settings "$hub" | grep -c -x 'health: {},')" != "1" ]]; then
  fail "hub.json5 must serve a status page on the client listener: CI's probe asks it"
fi
# The check must never act on what it finds: no command that restarts, stops
# or signals something, at the start of a line or after a shell operator.
if grep -v -E '^[[:space:]]*#' "$health" |
  grep -q -E '(^|\|\||&&|;|\$\()[[:space:]]*(systemctl|launchctl|kill|pkill|killall)[[:space:]]'; then
  fail "hub-health.sh must only report: it must not restart, stop or signal anything"
fi
if cat "$health_unit" "$hub_unit" "$health_timer" | grep -v -E '^[[:space:]]*#' |
  grep -q -E 'OnFailure|RuntimeMaxSec|WatchdogSec|OnCalendar'; then
  fail "no unit may restart the hub on a schedule or on a failed health check"
fi

# ---------------------------------------------------------------------------
# One NativeLink version: the build script, the patch and the runbook agree.
# ---------------------------------------------------------------------------

version="$(sed -n -E 's/^NATIVELINK_VERSION="([0-9.]+)"$/\1/p' "$build")"
commit="$(sed -n -E 's/^NATIVELINK_COMMIT="([0-9a-f]{40})"$/\1/p' "$build")"
marker="$(sed -n -E 's/^PATCH_MARKER="([a-z-]+)"$/\1/p' "$build")"
if [[ -z "$version" || -z "$commit" || -z "$marker" ]]; then
  fail "build-worker.sh must pin NATIVELINK_VERSION, NATIVELINK_COMMIT and PATCH_MARKER"
fi
grep -q -x "Applies to NativeLink v${version} (${commit})." "$patch" ||
  fail "the patch does not say it applies to v${version} (${commit})"
grep -q -x "+    version = \"${version}+${marker}\"," "$patch" ||
  fail "the patch does not give the binary the version ${version}+${marker} that build-worker.sh checks for"
grep -q -F "nativelink ${version}+${marker}" "$readme" ||
  fail "README.md does not name the version a worker reports (nativelink ${version}+${marker})"
grep -q -F "$commit" "$readme" ||
  fail "README.md does not name the pinned commit ${commit}"
for file in "$hub" "$worker"; do
  grep -q -F "$version" "$file" || fail "$(basename "$file") does not say which NativeLink version it is for (${version})"
done

# ---------------------------------------------------------------------------
# No address is written into the repository.
# ---------------------------------------------------------------------------

for file in "$hub" "$worker" "$plist" "$hub_unit" "$health_unit" "$health_timer" "$readme"; do
  # Loopback is the one address that is the same on every host.
  if grep -o -E '([0-9]{1,3}\.){3}[0-9]{1,3}' "$file" | grep -v -x -q '127\.0\.0\.1'; then
    fail "$(basename "$file") contains an IP address; addresses are host configuration"
  fi
  if grep -q -E 'grpcs?://[a-z0-9]' "$file"; then
    fail "$(basename "$file") names a remote endpoint"
  fi
done

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
