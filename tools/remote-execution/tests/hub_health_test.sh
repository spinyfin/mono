#!/bin/bash
#
# Tests for nativelink/hub-health.sh, the check that tells a hub that is
# scheduling from one that is only running.
#
# The script is run as it runs on the hub, as a process, against a stub `curl`
# on PATH and a log file. The worker and queue listings and the log events are
# what a NativeLink 1.8.6 hub produced (admin API bodies and NL_LOG=json lines
# captured from one, with ids shortened); only the numbers and timestamps are
# varied here.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
health="${workspace}/tools/remote-execution/nativelink/hub-health.sh"
tmp="${TEST_TMPDIR:?}"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# ---------------------------------------------------------------------------
# The hub, as the check sees it.
# ---------------------------------------------------------------------------

hub="${tmp}/hub"
mkdir -p "${hub}" "${tmp}/bin"

# curl: answers from the files in ${hub}. `<name>.code` is the HTTP status
# (000: nothing answered, and curl fails), `<name>.body` the body. Written
# with printf and not a here-document: the bash macOS ships puts those in a
# temporary file outside TMPDIR, which the test sandbox denies.
# shellcheck disable=SC2016  # the stub's own variables, expanded when it runs
printf '%s\n' \
  '#!/bin/bash' \
  'output=/dev/null' \
  'url=""' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    --output) output="$2"; shift 2 ;;' \
  '    --max-time | --write-out) shift 2 ;;' \
  '    --silent) shift ;;' \
  '    *) url="$1"; shift ;;' \
  '  esac' \
  'done' \
  'name="${url##*/}"' \
  'echo "$url" >> "${HUB_FIXTURES}/requests"' \
  'code="$(cat "${HUB_FIXTURES}/${name}.code" 2> /dev/null || echo 200)"' \
  'printf "%s" "$code"' \
  'if [[ "$code" == "000" ]]; then' \
  '  exit 28' \
  'fi' \
  'cat "${HUB_FIXTURES}/${name}.body" > "$output" 2> /dev/null || true' \
  > "${tmp}/bin/curl"
chmod +x "${tmp}/bin/curl"
# journalctl: --quiet must be passed, or the empty-window banner would make
# an idle hub look like a non-JSON log. With --quiet, no entries is silence.
# shellcheck disable=SC2016
printf '%s\n' \
  '#!/bin/bash' \
  'echo "$*" >> "${HUB_FIXTURES}/journalctl-args"' \
  'quiet=0' \
  'while [[ $# -gt 0 ]]; do' \
  '  case "$1" in' \
  '    --quiet) quiet=1; shift ;;' \
  '    *) shift ;;' \
  '  esac' \
  'done' \
  'if [[ "$quiet" -ne 1 ]]; then' \
  '  echo "-- No entries --"' \
  '  exit 0' \
  'fi' \
  'if [[ -f "${HUB_FIXTURES}/journal.log" ]]; then' \
  '  cat "${HUB_FIXTURES}/journal.log"' \
  'fi' \
  > "${tmp}/bin/journalctl"
chmod +x "${tmp}/bin/journalctl"
export HUB_FIXTURES="${hub}"
export PATH="${tmp}/bin:${PATH}"

status_ok='[{"namespace":"/nativelink/stores/CAS_MAIN_STORE/FilesystemStore","status":{"Ok":{"struct_name":"nativelink_store::filesystem_store::FilesystemStore","message":"FilesystemStore::check_health: ok"}}}]'

# worker <name> <running> <max> <paused> <draining>: one entry of the worker
# listing.
worker() {
  printf '{"id":"%s-1f1e39f0-5c8d-6b6c-929e-f6eb2520d280","running_actions":%s,"max_inflight_tasks":%s,"is_paused":%s,"is_draining":%s,"last_update_timestamp":1795040758,"platform_properties":{"cpu_count":"%s","container-image":"","OSFamily":""},"available_platform_properties":{"container-image":"","OSFamily":"","cpu_count":"%s"},"free_memory_kb":null}' \
    "$1" "$2" "$3" "$4" "$5" "$3" "$(($3 - $2))"
}

# set_workers <entry>...: the listing the hub answers with.
set_workers() {
  local IFS=,
  printf '[%s]' "$*" > "${hub}/workers.body"
}

# The check reads the real clock, so ages are relative to `now`, which
# reset_hub takes afresh for each case. Every age below is far from the limit
# it is compared with, and no assertion names an age to the second: a loaded
# machine can take seconds between writing a fixture and the check reading it.
now="$(date +%s)"

# set_queue <seconds queued>...: one queued action per argument.
set_queue() {
  local entries=() age
  for age in "$@"; do
    entries+=("{\"operation_id\":\"52eeadfb-9b7e-4633-8bfb-4df45f3e3fd9\",\"queued_since_ms\":$(((now - age) * 1000)),\"priority\":0,\"platform_properties\":{},\"client_last_seen_ms\":$((now * 1000))}")
  done
  local IFS=,
  printf '[%s]' "${entries[*]-}" > "${hub}/demand.body"
}

# stamp <seconds ago>: a log timestamp.
stamp() {
  local at=$((now - $1))
  date -u -d "@${at}" '+%Y-%m-%dT%H:%M:%S.000000Z' 2> /dev/null || date -u -r "${at}" '+%Y-%m-%dT%H:%M:%S.000000Z'
}

log="${tmp}/hub.log"

# event <seconds ago> <level> <message> [<more fields, as JSON members>]
event() {
  printf '{"timestamp":"%s","level":"%s","fields":{"message":"%s"%s},"target":"nativelink_scheduler"}\n' \
    "$(stamp "$1")" "$2" "$3" "${4:+,$4}" >> "$log"
}

timed_out() { event "$1" WARN "Worker timed out, removing from pool" '"worker_id":"probe1-1f1e39f0"'; }
kill_ignored() { event "$1" WARN "Worker did not acknowledge a kill in time, removing from pool" '"worker_id":"probe1-1f1e39f0","unacknowledged_kill_timeout_s":60'; }
# A worker that went away by itself, which is what a restart of it looks like.
left() { event "$1" INFO "Evicting worker from pool" '"worker_id":"probe1-1f1e39f0","is_disconnect":true,"running_actions":0,"reason":"Worker stream ended without going away"'; }
operation_timed_out() { event "$1" WARN "Timing out operation" '"operation_id":"0eae26f4","worker_id":"Some(probe1-1f1e39f0)"'; }
result_lost() { event "$1" WARN "Executing operation is not running on its worker; its result was lost, requeuing" '"operation_id":"0eae26f4"'; }
unreported() { event "$1" WARN "Worker no longer reports an operation it acknowledged and sent no result for; requeuing" '"operation_id":"0eae26f4"'; }

# A pool of six with nothing queued, a status page that answers, and a log
# with one ordinary line in it.
reset_hub() {
  now="$(date +%s)"
  rm -f "${hub}"/*.code "${hub}/requests" "${hub}/journalctl-args" "${hub}/journal.log" "${tmp}/health.state" "${tmp}/alerts"
  printf '%s' "$status_ok" > "${hub}/status.body"
  set_workers \
    "$(worker anaplian 3 10 false false)" "$(worker arbitrary 0 12 false false)" \
    "$(worker determinist 12 12 false false)" "$(worker rascal 1 12 false false)" \
    "$(worker skaffen 0 10 false false)" "$(worker zakalwe 2 10 false false)"
  set_queue
  : > "$log"
  event 30 INFO "Client connected" '"remote_addr":"127.0.0.1:58643"'
}

# The check keeps its scratch files under TMPDIR, as it does on the hub. In
# the test sandbox only the test's own directory is writable.
export TMPDIR="${tmp}"

# check [NAME=value]...: runs the check with the hub's settings and the given
# extra environment. Sets `line` to what it printed and `status` to its exit
# status.
check() {
  status=0
  line="$(env NL_HUB_ADMIN_LISTEN=127.0.0.1:50071 NL_HEALTH_EXPECTED_WORKERS=6 \
    NL_HEALTH_STATE_FILE="${tmp}/health.state" "$@" "$health" --log-file "$log" 2>&1)" || status=$?
}

# check_journal [NAME=value]...: the same, reading the log from the stub
# journalctl instead of a file.
check_journal() {
  status=0
  : > "${hub}/journalctl-args"
  line="$(env NL_HUB_ADMIN_LISTEN=127.0.0.1:50071 NL_HEALTH_EXPECTED_WORKERS=6 \
    NL_HEALTH_STATE_FILE="${tmp}/health.state" "$@" "$health" \
    --journal-unit nativelink-macos-hub.service 2>&1)" || status=$?
}

# expect <description> <exit status> <substring of the line>...
expect() {
  local description="$1" want_status="$2" part
  shift 2
  if [[ "$status" != "$want_status" ]]; then
    fail "${description}: expected exit ${want_status}, got ${status}: ${line}"
  fi
  for part in "$@"; do
    if [[ "$line" != *"$part"* ]]; then
      fail "${description}: expected '${part}' in: ${line}"
    fi
  done
}

# ---------------------------------------------------------------------------
# A hub that is scheduling.
# ---------------------------------------------------------------------------

reset_hub
check
expect "healthy" 0 "healthy: 6/6 workers, 18 of 66 slots busy, 0 queued" "0 forced evictions, 0 operation timeouts, 0 lost results"

# A busy pool is not an unhealthy one: actions wait, but every slot is taken.
reset_hub
set_workers "$(worker anaplian 10 10 false false)" "$(worker arbitrary 12 12 false false)" \
  "$(worker determinist 12 12 false false)" "$(worker rascal 12 12 false false)" \
  "$(worker skaffen 10 10 false false)" "$(worker zakalwe 10 10 false false)"
set_queue 900 700 20
check
expect "saturated" 0 "healthy:" "66 of 66 slots busy, 3 queued (oldest 9"

# One worker lost, and a worker restarted, in ten minutes is a day like any
# other.
reset_hub
timed_out 120
left 300
left 60
operation_timed_out 110
check
expect "one eviction" 0 "healthy:" "1 forced evictions, 1 operation timeouts"

# ---------------------------------------------------------------------------
# The loop of 2026-11-18: operations timed out, their workers evicted.
# ---------------------------------------------------------------------------

reset_hub
timed_out 400
kill_ignored 200
timed_out 20
check
expect "eviction loop" 1 "UNHEALTHY:" "3 workers were evicted for being silent or for not acknowledging a kill in the last 600s (limit 2)"

# The same three, but before the window.
reset_hub
timed_out 2000
kill_ignored 1500
timed_out 700
check
expect "evictions before the window" 0 "healthy:" "0 forced evictions"

reset_hub
for ago in 300 250 200 150 100; do operation_timed_out "$ago"; done
check
expect "operation timeouts" 1 "the scheduler timed out 5 actions in the last 600s (limit 4)"

reset_hub
for ago in 300 250 200; do result_lost "$ago"; done
for ago in 150 100; do unreported "$ago"; done
check
expect "lost results" 1 "5 actions were queued again because their worker no longer had them"

# The limits are the hub's to set.
reset_hub
timed_out 400
kill_ignored 200
timed_out 20
check NL_HEALTH_MAX_FORCED_EVICTIONS=3
expect "raised limit" 0 "healthy:" "3 forced evictions"
check NL_HEALTH_WINDOW_S=100
expect "shorter window" 0 "healthy:" "last 100s: 1 forced evictions"

# ---------------------------------------------------------------------------
# Workers that are gone, and a queue that does not move.
# ---------------------------------------------------------------------------

reset_hub
set_workers "$(worker anaplian 3 10 false false)" "$(worker arbitrary 0 12 false false)" \
  "$(worker rascal 1 12 false false)" "$(worker skaffen 0 10 false false)"
check
expect "two workers missing" 1 "UNHEALTHY: 4 of 6 workers are connected" "4/6 workers"
# With no expected count only an empty pool is a fault.
check NL_HEALTH_EXPECTED_WORKERS=
expect "no expected count" 0 "healthy: 4 workers"

reset_hub
set_workers
check
expect "no workers" 1 "UNHEALTHY: no worker is connected" "0/6 workers, 0 of 0 slots busy"

# Actions waiting for minutes while slots are free: nothing a client can see
# distinguishes this from a long build.
reset_hub
set_queue 400 30
check
expect "stuck queue" 1 "2 actions are queued, the oldest for 4" "s, while 48 slots are free (limit 300s)"
# A slot on a paused or draining worker is not one the scheduler may use.
reset_hub
set_workers "$(worker anaplian 10 10 false false)" "$(worker arbitrary 0 12 true false)" \
  "$(worker determinist 12 12 false false)" "$(worker rascal 0 12 false true)" \
  "$(worker skaffen 10 10 false false)" "$(worker zakalwe 10 10 false false)"
set_queue 400
check
expect "free slots only on paused and draining workers" 0 "healthy:" "42 of 66 slots busy, 1 queued (oldest 4"
# Just queued is not stuck.
reset_hub
set_queue 30
check
expect "fresh queue" 0 "healthy:" "1 queued (oldest "

# ---------------------------------------------------------------------------
# A hub that does not answer, or says a store has failed.
# ---------------------------------------------------------------------------

reset_hub
echo 000 > "${hub}/status.code"
check
expect "no answer" 1 "UNHEALTHY: the hub did not answer a status request within 10s" "?/6 workers"
if grep -q admin "${hub}/requests"; then
  fail "no answer: the admin API was asked after the status page did not answer"
fi

reset_hub
echo 503 > "${hub}/status.code"
check
expect "failed store" 1 "the hub reports a failed or stalled store (its status page answers 503)" "6/6 workers"

reset_hub
echo 404 > "${hub}/workers.code"
check
expect "no admin API" 1 "the hub's admin API did not answer (workers: HTTP 404, queue: HTTP 200)"

# ---------------------------------------------------------------------------
# The check must not report a scheduler it cannot see as a quiet one.
# ---------------------------------------------------------------------------

reset_hub
printf '  2026-11-18T22:27:58.269004Z  WARN nativelink_scheduler: Worker timed out, removing from pool, worker_id: probe1\n' > "$log"
check
expect "log in another format" 2 "the hub's log has no JSON events"

reset_hub
printf '%s\n' '-- No entries --' > "$log"
check
expect "empty-window banner in a log file" 0 "healthy: 6/6 workers"

reset_hub
check_journal
expect "idle journal" 0 "healthy: 6/6 workers"
if ! grep -q -- '--quiet' "${hub}/journalctl-args"; then
  fail "idle journal: journalctl was not passed --quiet: $(cat "${hub}/journalctl-args")"
fi
# The hub is a system unit: its journal is read without --user.
if grep -q -- '--user' "${hub}/journalctl-args"; then
  fail "idle journal: journalctl was passed --user, but the hub is a system unit: $(cat "${hub}/journalctl-args")"
fi
cp "$log" "${hub}/journal.log"
check_journal
expect "journal with JSON" 0 "healthy: 6/6 workers" "1 log lines"

reset_hub
status=0
line="$(env NL_HUB_ADMIN_LISTEN=127.0.0.1:50071 NL_HEALTH_STATE_FILE="${tmp}/health.state" \
  "$health" --log-file "${tmp}/no-such-log" 2>&1)" || status=$?
expect "missing log" 2 "could not read the hub's log"

reset_hub
: > "${tmp}/alerts"
alert="printf '%s|%s\n' \"\$NL_HEALTH_STATE\" \"\$NL_HEALTH_LINE\" >> '${tmp}/alerts'"
status=0
line="$(env NL_HUB_ADMIN_LISTEN=127.0.0.1:50071 NL_HEALTH_STATE_FILE="${tmp}/health.state" \
  NL_HEALTH_ALERT_CMD="$alert" "$health" --log-file "${tmp}/no-such-log" 2>&1)" || status=$?
expect "missing log alerts" 2 "could not read the hub's log"
if ! grep -q '^error|hub-health: error: could not read the hub' "${tmp}/alerts"; then
  fail "missing log: expected an error alert, got: $(cat "${tmp}/alerts" 2>/dev/null)"
fi

status=0
line="$(env -u NL_HUB_ADMIN_LISTEN "$health" --log-file "$log" 2>&1)" || status=$?
expect "no admin address" 2 "NL_HUB_ADMIN_LISTEN is not set"

# A configuration error alerts once: state_file is set before validation, so
# a missing NL_HUB_ADMIN_LISTEN does not fire the hook every minute.
: > "${tmp}/alerts"
alert="printf '%s|%s\n' \"\$NL_HEALTH_STATE\" \"\$NL_HEALTH_LINE\" >> '${tmp}/alerts'"
rm -f "${tmp}/health.state"
status=0
line="$(env -u NL_HUB_ADMIN_LISTEN NL_HEALTH_STATE_FILE="${tmp}/health.state" \
  NL_HEALTH_ALERT_CMD="$alert" "$health" --log-file "$log" 2>&1)" || status=$?
expect "no admin address alerts" 2 "NL_HUB_ADMIN_LISTEN is not set"
if ! grep -q '^error|hub-health: error: NL_HUB_ADMIN_LISTEN is not set' "${tmp}/alerts"; then
  fail "no admin address: expected an error alert, got: $(cat "${tmp}/alerts" 2>/dev/null)"
fi
status=0
line="$(env -u NL_HUB_ADMIN_LISTEN NL_HEALTH_STATE_FILE="${tmp}/health.state" \
  NL_HEALTH_ALERT_CMD="$alert" "$health" --log-file "$log" 2>&1)" || status=$?
expect "no admin address alerts again" 2 "NL_HUB_ADMIN_LISTEN is not set"
if [[ "$(wc -l < "${tmp}/alerts" | tr -d ' ')" != "1" ]]; then
  fail "no admin address: expected one alert for two runs, got: $(cat "${tmp}/alerts")"
fi

status=0
line="$(env NL_HUB_ADMIN_LISTEN=127.0.0.1:50071 "$health" 2>&1)" || status=$?
expect "no log source" 2 "one of --journal-unit or --log-file is required"

reset_hub
check NL_HEALTH_MAX_QUEUE_AGE_S=soon
expect "a limit that is not a number" 2 "a limit is not a whole number"

# ---------------------------------------------------------------------------
# The alert runs when the state changes, and only then.
# ---------------------------------------------------------------------------

reset_hub
alert="printf '%s|%s\n' \"\$NL_HEALTH_STATE\" \"\$NL_HEALTH_LINE\" >> '${tmp}/alerts'"
check NL_HEALTH_ALERT_CMD="$alert"
expect "alert: healthy from the start" 0 "healthy:"
if [[ -e "${tmp}/alerts" ]]; then
  fail "alert: ran for a hub that was healthy from the first run: $(cat "${tmp}/alerts")"
fi
set_workers
check NL_HEALTH_ALERT_CMD="$alert"
check NL_HEALTH_ALERT_CMD="$alert"
expect "alert: still unhealthy" 1 "no worker is connected"
if [[ "$(wc -l < "${tmp}/alerts" | tr -d ' ')" != "1" ]]; then
  fail "alert: expected one alert for two unhealthy runs, got: $(cat "${tmp}/alerts")"
fi
if ! grep -q '^unhealthy|UNHEALTHY: no worker is connected' "${tmp}/alerts"; then
  fail "alert: the alert does not carry the state and the line: $(cat "${tmp}/alerts")"
fi
set_workers "$(worker anaplian 0 10 false false)" "$(worker arbitrary 0 12 false false)" \
  "$(worker determinist 0 12 false false)" "$(worker rascal 0 12 false false)" \
  "$(worker skaffen 0 10 false false)" "$(worker zakalwe 0 10 false false)"
check NL_HEALTH_ALERT_CMD="$alert"
expect "alert: recovered" 0 "healthy:"
if [[ "$(wc -l < "${tmp}/alerts" | tr -d ' ')" != "2" ]] || ! grep -q '^healthy|healthy: 6/6 workers' "${tmp}/alerts"; then
  fail "alert: expected a second alert on recovery, got: $(cat "${tmp}/alerts")"
fi
# An alert command that fails does not change the verdict.
set_workers
check NL_HEALTH_ALERT_CMD="exit 3"
expect "alert: failing command" 1 "no worker is connected"

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
