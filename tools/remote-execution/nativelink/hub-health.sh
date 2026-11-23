#!/bin/bash
# hub-health.sh -- is the NativeLink hub scheduling, or only running?
#
# A hub can accept connections, answer its status page and still finish no
# action. On 2026-11-18 one did for hours: it timed operations out, evicted
# the workers they ran on, the workers reconnected, and it started again,
# while every Bazel client waited without an error. Nothing reported it. This
# check does, and it reports the two states that look the same from a client:
# workers that are gone, and a queue that does not move although workers are
# free.
#
# It runs on the hub's host once a minute (nativelink-macos-hub-health.timer)
# and prints one line. Exit status: 0 healthy, 1 unhealthy, 2 the check could
# not run. The unit therefore shows as failed while the hub is unhealthy, and
# NL_HEALTH_ALERT_CMD, if set, runs when the state changes. The check never
# restarts or stops anything: ../README.md, "Recovery", is for a person.
#
#   hub-health.sh --journal-unit <unit>   read the hub's log from the system journal
#                                         (--quiet, so an empty window is
#                                         silence and not "-- No entries --")
#   hub-health.sh --log-file <path>       read it from a file
#
# The log has to be NativeLink's JSON format (NL_LOG=json, set by
# nativelink-macos-hub.service): one event per line, with its own timestamp.
#
# Environment. The first is the hub's own setting, from hub.env; the others
# are optional and belong in hub.env too.
#
#   NL_HUB_ADMIN_LISTEN         127.0.0.1:<port>, as in hub.json5.
#   NL_HEALTH_EXPECTED_WORKERS  How many workers the pool has. Fewer connected
#                               is unhealthy. Unset: only none at all is.
#   NL_HEALTH_ALERT_CMD         Run with `sh -c` when the hub becomes unhealthy,
#                               when it recovers, and when the check cannot run.
#                               It gets the line this script prints in
#                               NL_HEALTH_LINE and "unhealthy", "healthy" or
#                               "error" in NL_HEALTH_STATE.
#   NL_HEALTH_STATE_FILE        Where the last state is kept between runs.
#                               Default: health.state next to this script.
#
# The limits. Each is what an ordinary day does not reach; the reasons are in
# ../README.md, "Health check".
#
#   NL_HEALTH_WINDOW_S            600  How far back the log is read.
#   NL_HEALTH_MAX_FORCED_EVICTIONS  2  Workers evicted in the window for being
#                                      silent or for not acknowledging a kill.
#                                      A worker that disconnects by itself (a
#                                      restart) is not counted.
#   NL_HEALTH_MAX_OPERATION_TIMEOUTS 4 Actions the scheduler timed out.
#   NL_HEALTH_MAX_LOST_RESULTS      4  Actions queued again because their
#                                      worker no longer had them.
#   NL_HEALTH_MAX_QUEUE_AGE_S     300  Age of the oldest queued action while
#                                      some worker has a free slot.
set -u

# The scheduler's name in hub.json5; the admin API is addressed by it.
SCHEDULER_NAME="MAIN_SCHEDULER"
# How long the hub gets to answer one request. Its own check of a store gives
# up after 5 s and then answers 503, so a hub that says nothing for 10 s is
# not answering at all.
REQUEST_TIMEOUT_S=10

# Set before any die() so a repeating configuration error (a missing
# NL_HUB_ADMIN_LISTEN, a bad argument) alerts once, not every minute. The
# default lives next to this script on the hub; tests pass NL_HEALTH_STATE_FILE.
state_file="${NL_HEALTH_STATE_FILE:-$(dirname "${BASH_SOURCE[0]}")/health.state}"

die() {
  echo "hub-health: error: $*" >&2
  # A check that could not run is as much a signal as an unhealthy hub.
  # state_file is set before argument and environment validation, so every
  # die() goes through the same dedup path as an unhealthy verdict.
  if [[ -n "${NL_HEALTH_ALERT_CMD:-}" ]]; then
    previous=healthy
    if [[ -n "${state_file:-}" ]]; then
      previous="$(cat "$state_file" 2> /dev/null || echo healthy)"
    fi
    if [[ "$previous" != "error" ]]; then
      if [[ -n "${state_file:-}" ]]; then
        echo error > "$state_file" 2> /dev/null || true
      fi
      NL_HEALTH_LINE="hub-health: error: $*" NL_HEALTH_STATE="error" sh -c "$NL_HEALTH_ALERT_CMD" ||
        echo "hub-health: warning: NL_HEALTH_ALERT_CMD failed" >&2
    fi
  fi
  exit 2
}

source_kind=""
source_arg=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --journal-unit | --log-file)
      [[ $# -ge 2 ]] || die "$1 needs a value"
      source_kind="$1"
      source_arg="$2"
      shift 2
      ;;
    *) die "unknown argument: $1" ;;
  esac
done
[[ -n "$source_kind" ]] || die "one of --journal-unit or --log-file is required"
[[ -n "${NL_HUB_ADMIN_LISTEN:-}" ]] || die "NL_HUB_ADMIN_LISTEN is not set"

window_s="${NL_HEALTH_WINDOW_S:-600}"
max_forced_evictions="${NL_HEALTH_MAX_FORCED_EVICTIONS:-2}"
max_operation_timeouts="${NL_HEALTH_MAX_OPERATION_TIMEOUTS:-4}"
max_lost_results="${NL_HEALTH_MAX_LOST_RESULTS:-4}"
max_queue_age_s="${NL_HEALTH_MAX_QUEUE_AGE_S:-300}"
expected_workers="${NL_HEALTH_EXPECTED_WORKERS:-}"
for value in "$window_s" "$max_forced_evictions" "$max_operation_timeouts" "$max_lost_results" "$max_queue_age_s" "${expected_workers:-0}"; do
  [[ "$value" =~ ^[0-9]+$ ]] || die "a limit is not a whole number: '$value'"
done

admin="http://${NL_HUB_ADMIN_LISTEN}"
now_s="$(date +%s)"

problems=()

# ---------------------------------------------------------------------------
# 1. Does the hub answer, and does it think its stores work?
# ---------------------------------------------------------------------------

# http_get <path> <body file>: prints the HTTP status, 000 for no answer.
http_get() {
  curl --silent --max-time "$REQUEST_TIMEOUT_S" --output "$2" --write-out '%{http_code}' "${admin}$1" 2> /dev/null || true
}

# An explicit template: given none, the mktemp macOS ships ignores TMPDIR.
scratch="$(mktemp -d "${TMPDIR:-/tmp}/hub-health.XXXXXX")" || die "could not create a temporary directory"
trap 'rm -rf "$scratch"' EXIT

answers=1
status_code="$(http_get /status "$scratch/status")"
case "$status_code" in
  200) ;;
  503) problems+=("the hub reports a failed or stalled store (its status page answers 503)") ;;
  000 | "")
    answers=0
    problems+=("the hub did not answer a status request within ${REQUEST_TIMEOUT_S}s")
    ;;
  *) problems+=("the hub's status page answered HTTP ${status_code}") ;;
esac

# ---------------------------------------------------------------------------
# 2. Workers and queue, from the admin API.
# ---------------------------------------------------------------------------

workers="?"
busy_slots="?"
total_slots="?"
free_slots=0
queued="?"
oldest_queued_s=0
if [[ "$answers" == 1 ]]; then
  workers_code="$(http_get "/admin/scheduler/${SCHEDULER_NAME}/workers" "$scratch/workers")"
  demand_code="$(http_get "/admin/scheduler/${SCHEDULER_NAME}/demand" "$scratch/demand")"
  if [[ "$workers_code" != 200 || "$demand_code" != 200 ]]; then
    problems+=("the hub's admin API did not answer (workers: HTTP ${workers_code:-000}, queue: HTTP ${demand_code:-000})")
  else
    # One match per worker. The fields are in the order NativeLink writes
    # them; a release that changes it makes this count zero workers, which
    # reads as unhealthy and not as fine.
    read -r workers busy_slots total_slots free_slots < <(
      grep -o '"running_actions":[0-9]*,"max_inflight_tasks":[0-9]*,"is_paused":[a-z]*,"is_draining":[a-z]*' "$scratch/workers" |
        awk -F'[:,]' '
          { n++; busy += $2; total += $4; if ($6 == "false" && $8 == "false" && $4 > $2) free += $4 - $2 }
          END { printf "%d %d %d %d\n", n, busy, total, free }'
    )
    read -r queued oldest_queued_ms < <(
      grep -o '"queued_since_ms":[0-9]*' "$scratch/demand" |
        awk -F: 'NR == 1 || $2 < oldest { oldest = $2 } { n++ } END { printf "%d %d\n", n, oldest }'
    )
    if ((queued > 0)); then
      oldest_queued_s=$((now_s - oldest_queued_ms / 1000))
      ((oldest_queued_s >= 0)) || oldest_queued_s=0
    fi

    if ((workers == 0)); then
      problems+=("no worker is connected")
    elif [[ -n "$expected_workers" ]] && ((workers < expected_workers)); then
      problems+=("${workers} of ${expected_workers} workers are connected")
    fi
    if ((queued > 0 && free_slots > 0 && oldest_queued_s > max_queue_age_s)); then
      problems+=("${queued} actions are queued, the oldest for ${oldest_queued_s}s, while ${free_slots} slots are free (limit ${max_queue_age_s}s)")
    fi
  fi
fi

# ---------------------------------------------------------------------------
# 3. What the scheduler did in the window, from its log.
# ---------------------------------------------------------------------------

read_log() {
  case "$source_kind" in
    --journal-unit) journalctl --quiet --unit "$source_arg" --since "-${window_s}s" --output cat --no-pager ;;
    --log-file) cat "$source_arg" ;;
  esac
}

# Timestamps are UTC, in a form that sorts as text, so "in the window" is a
# string comparison. GNU date and the BSD one spell "this many seconds ago"
# differently.
since_s=$((now_s - window_s))
since="$(date -u -d "@${since_s}" '+%Y-%m-%dT%H:%M:%S' 2> /dev/null || date -u -r "${since_s}" '+%Y-%m-%dT%H:%M:%S')" ||
  die "could not work out the start of the window"

# Read through a process substitution, as the listings above are: a here-string
# needs a temporary file outside TMPDIR on the bash macOS ships, which the
# sandbox the tests run in does not allow.
read -r raw_lines log_events log_lines forced_evictions operation_timeouts lost_results < <(
  read_log 2> "$scratch/log-error" | awk -v since="$since" '
    # {"timestamp":"2026-11-18T22:27:36.123456Z","level":"WARN","fields":{"message":"..."
    # journalctl without --quiet prints "-- No entries --" for an empty
    # window; skip that banner so an idle hub is not reported as "no JSON".
    /^-- / { next }
    {
      lines++
      if (!match($0, /"timestamp":"[0-9T:.-]+Z"/)) next
      events++
      if (substr($0, RSTART + 13, 19) < since) next
      seen++
    }
    /"message":"Worker timed out, removing from pool"/ { evicted++ }
    /"message":"Worker did not acknowledge a kill in time, removing from pool"/ { evicted++ }
    /"message":"Timing out operation"/ { timed_out++ }
    /"message":"Executing operation is not running on its worker; its result was lost, requeuing"/ { lost++ }
    /"message":"Worker no longer reports an operation it acknowledged and sent no result for; requeuing"/ { lost++ }
    END { printf "%d %d %d %d %d %d\n", lines, events, seen, evicted, timed_out, lost }'
)
if [[ -s "$scratch/log-error" ]]; then
  die "could not read the hub's log: $(head -1 "$scratch/log-error")"
fi
# A log in another format would count as a quiet scheduler. Refuse it instead.
if ((raw_lines > 0 && log_events == 0)); then
  die "the hub's log has no JSON events; is NL_LOG=json set for the hub?"
fi

if ((forced_evictions > max_forced_evictions)); then
  problems+=("${forced_evictions} workers were evicted for being silent or for not acknowledging a kill in the last ${window_s}s (limit ${max_forced_evictions})")
fi
if ((operation_timeouts > max_operation_timeouts)); then
  problems+=("the scheduler timed out ${operation_timeouts} actions in the last ${window_s}s (limit ${max_operation_timeouts})")
fi
if ((lost_results > max_lost_results)); then
  problems+=("${lost_results} actions were queued again because their worker no longer had them in the last ${window_s}s (limit ${max_lost_results})")
fi

# ---------------------------------------------------------------------------
# Report.
# ---------------------------------------------------------------------------

facts="${workers}${expected_workers:+/${expected_workers}} workers, ${busy_slots} of ${total_slots} slots busy, ${queued} queued"
if [[ "$queued" != "?" ]] && ((queued > 0)); then
  facts+=" (oldest ${oldest_queued_s}s)"
fi
facts+="; last ${window_s}s: ${forced_evictions} forced evictions, ${operation_timeouts} operation timeouts, ${lost_results} lost results, ${log_lines} log lines"

if ((${#problems[@]} == 0)); then
  state=healthy
  line="healthy: ${facts}"
else
  state=unhealthy
  joined="$(printf '%s; ' "${problems[@]}")"
  line="UNHEALTHY: ${joined%; } | ${facts}"
fi
echo "$line"

previous="$(cat "$state_file" 2> /dev/null || echo healthy)"
if [[ "$state" != "$previous" ]]; then
  # A state that cannot be saved must not be lost quietly: the next run would
  # alert again, or never.
  echo "$state" > "$state_file" || die "could not write ${state_file}"
  if [[ -n "${NL_HEALTH_ALERT_CMD:-}" ]]; then
    NL_HEALTH_LINE="$line" NL_HEALTH_STATE="$state" sh -c "$NL_HEALTH_ALERT_CMD" ||
      echo "hub-health: warning: NL_HEALTH_ALERT_CMD failed" >&2
  fi
fi

[[ "$state" == healthy ]]
