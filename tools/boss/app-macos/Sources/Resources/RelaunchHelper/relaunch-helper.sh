#!/bin/sh
# relaunch-helper.sh — relaunch Boss into a freshly-swapped bundle and watchdog its
# first launch, leaving a durable trail of every step.
#
# The bundle swap itself (current bundle -> .bak, staged bundle -> install location)
# is performed *in-process* by UpdateCore's `UpdateInstaller.applySwap` before this
# helper is spawned. This script handles only the parts that cannot run inside the
# Boss process once it has exited:
#
#   1. wait for the old Boss PID to exit (clean cold relaunch). There is no give-up:
#      `open` on a bundle whose previous process is still registered with
#      LaunchServices merely activates that old process and launches nothing, so
#      proceeding early would always look like a failed launch and roll back a good
#      install; and exiting would leave the untried bundle installed with no
#      first-launch watchdog. A slow quit is logged every `--old-exit-timeout`
#      seconds (default 300) and the wait continues.
#   2. assess the swapped bundle (version, executable, quarantine, codesign),
#   3. `open` the new bundle and confirm a *new* process running the installed
#      executable actually appeared (pid, version, path executed),
#   4. watchdog its first launch — timed from the moment that process was seen,
#      not from `open` — by polling for the first-launch-OK flag the new version
#      writes on `applicationDidFinishLaunching`,
#   5. on failure, record the reason (open failed / never started / exited early /
#      handshake timeout), terminate a still-running new process, restore the .bak,
#      reopen the previous version, and write a rolled-back marker the app reads on
#      its next launch to blocklist the bad version with that reason.
#
# Every step appends a JSON line to the update log (`--log`), tagged with the
# attempt id (`--attempt`) the app minted, so the helper's records interleave with
# the old app's swap records and the new app's handshake/reconcile records. Each
# record is also sent to the unified log through `logger` (tag
# `boss-relaunch-helper`), and the next app launch replays the helper's records into
# its own `update-lifecycle` os_log category.
#
# Backward compatibility: Boss 1.0.734 and earlier arm this script (the swapped-in
# bundle's copy) with only --pid/--install/--backup/--flag/--rolled-back-marker/
# --version/--watchdog. The log path, attempt id and `from` version are then derived
# (see "0. Start"), and the rolled-back marker keeps its legacy one-line format —
# just the failed version — because that old app is the one that reads it after a
# rollback. The reason and context go to a `<marker>.detail` sidecar.
#
# See tools/boss/docs/designs/automatic-boss-updates.md §4 and
# tools/boss/docs/update-attempt-log.md. Invoked as
#   /bin/sh relaunch-helper.sh --pid N --install P --backup P --flag P \
#     --rolled-back-marker P --version V --watchdog SECS --attempt ID --from V --log P
# (no +x bit required). Tools are resolved through PATH so tests can substitute
# `open`/`codesign`/`xattr`/`plutil`; everything else used here is POSIX sh or a
# base-system utility.

set -u
PATH="${PATH:-/usr/bin:/bin:/usr/sbin:/sbin}"
export PATH

pid=""
install=""
backup=""
flag=""
marker=""
version=""
watchdog=30
attempt=""
from=""
logfile=""
# How often to log that the old process is still running (the wait itself never
# gives up; see step 1).
old_exit_timeout="${BOSS_RELAUNCH_OLD_EXIT_TIMEOUT:-300}"
# How long after `open` to wait for a process running the installed bundle to appear.
launch_timeout="${BOSS_RELAUNCH_LAUNCH_TIMEOUT:-15}"

while [ $# -gt 0 ]; do
  case "$1" in
    --pid) pid="$2"; shift 2 ;;
    --install) install="$2"; shift 2 ;;
    --backup) backup="$2"; shift 2 ;;
    --flag) flag="$2"; shift 2 ;;
    --rolled-back-marker) marker="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    --watchdog) watchdog="$2"; shift 2 ;;
    --attempt) attempt="$2"; shift 2 ;;
    --from) from="$2"; shift 2 ;;
    --log) logfile="$2"; shift 2 ;;
    --old-exit-timeout) old_exit_timeout="$2"; shift 2 ;;
    --launch-timeout) launch_timeout="$2"; shift 2 ;;
    *) shift ;;
  esac
done

[ -n "$install" ] || exit 64  # nothing to relaunch

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

now_iso() { date -u +%Y-%m-%dT%H:%M:%SZ; }
now_s() { date +%s; }

# Escape a value for embedding in a JSON string: backslashes, quotes, newlines/tabs.
json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/	/\\t/g' | tr '\n' ' '
}

json_or_null() {
  if [ -n "$1" ]; then printf '"%s"' "$(json_escape "$1")"; else printf 'null'; fi
}

# log_event STAGE EVENT LEVEL [key=value ...]
# Writes one JSON line to the durable update log and the same line to the unified
# log via `logger` (two independent sinks: either can fail without losing the other).
# (POSIX sh has no `local`; every helper function uses a distinct variable prefix so
# nested calls — rollback() logging while holding its own detail — cannot clobber.)
log_event() {
  le_stage="$1"; le_event="$2"; le_level="$3"; shift 3
  le_detail=""
  for le_kv in "$@"; do
    le_k="${le_kv%%=*}"
    le_v="${le_kv#*=}"
    le_detail="${le_detail}${le_detail:+,}\"$(json_escape "$le_k")\":\"$(json_escape "$le_v")\""
  done
  le_line=$(printf '{"actor":"helper","attempt":%s,"detail":{%s},"event":"%s","from":%s,"level":"%s","pid":%s,"stage":"%s","to":%s,"ts":"%s"}' \
    "$(json_or_null "$attempt")" "$le_detail" "$le_event" "$(json_or_null "$from")" "$le_level" "$$" \
    "$le_stage" "$(json_or_null "$version")" "$(now_iso)")
  if [ -n "$logfile" ]; then
    mkdir -p "$(dirname "$logfile")" 2>/dev/null
    printf '%s\n' "$le_line" >> "$logfile"
  fi
  case "$le_level" in error) le_pri=user.err ;; warning) le_pri=user.warning ;; *) le_pri=user.notice ;; esac
  logger -p "$le_pri" -t boss-relaunch-helper -- "$le_line" >/dev/null 2>&1
}

# ---------------------------------------------------------------------------
# Bundle probes
# ---------------------------------------------------------------------------

# plist_string BUNDLE KEY — read a top-level string from Contents/Info.plist.
# `plutil` handles binary plists; the sed fallback covers XML when plutil is absent.
plist_string() {
  _plist="$1/Contents/Info.plist"
  [ -f "$_plist" ] || return 1
  _val=$(plutil -extract "$2" raw -o - "$_plist" 2>/dev/null) && [ -n "$_val" ] && { printf '%s' "$_val"; return 0; }
  _val=$(sed -n "/<key>$2<\/key>/{n;s/.*<string>\(.*\)<\/string>.*/\1/p;}" "$_plist" 2>/dev/null | head -1)
  [ -n "$_val" ] && { printf '%s' "$_val"; return 0; }
  return 1
}

executable_name() {
  plist_string "$1" CFBundleExecutable 2>/dev/null || printf 'Boss'
}

# find_running_pid PREFIX — pid of a process whose command line references
# PREFIX (the installed bundle's Contents/MacOS/), excluding the old Boss pid.
# The prefix travels through the environment, not argv: an `awk -v prefix=…`
# argument would show up in `ps` and match the awk process itself.
find_running_pid() {
  BOSS_RELAUNCH_MATCH_PREFIX="$1" BOSS_RELAUNCH_OLD_PID="$pid" \
  ps -axww -o pid=,command= 2>/dev/null | BOSS_RELAUNCH_MATCH_PREFIX="$1" BOSS_RELAUNCH_OLD_PID="$pid" BOSS_RELAUNCH_OTHER_PID="$other_pid" awk '
    {
      p = $1
      $1 = ""
      cmd = substr($0, 2)
      if (index(cmd, ENVIRON["BOSS_RELAUNCH_MATCH_PREFIX"]) > 0 && p != ENVIRON["BOSS_RELAUNCH_OLD_PID"] && index(" " ENVIRON["BOSS_RELAUNCH_OTHER_PID"] " ", " " p " ") == 0) { print p }
    }'
}

is_alive() { kill -0 "$1" 2>/dev/null; }

# ---------------------------------------------------------------------------
# Rollback
# ---------------------------------------------------------------------------

new_pid=""
other_pid=""

# rollback REASON DETAIL — restore the previous bundle, reopen it, record why.
rollback() {
  rb_reason="$1"; rb_detail="$2"
  log_event rollback decision error "reason=$rb_reason" "detail=$rb_detail" "install=$install" "backup=$backup"

  if [ -n "$new_pid" ] && is_alive "$new_pid"; then
    kill -TERM "$new_pid" 2>/dev/null
    rb_i=0
    while is_alive "$new_pid" && [ "$rb_i" -lt 50 ]; do rb_i=$((rb_i + 1)); sleep 0.1; done
    if is_alive "$new_pid"; then
      kill -KILL "$new_pid" 2>/dev/null
      log_event rollback terminated-new-process warning "pid=$new_pid" "signal=KILL"
    else
      log_event rollback terminated-new-process warning "pid=$new_pid" "signal=TERM"
    fi
  fi

  rb_restored=""
  if [ -n "$backup" ] && [ -d "$backup" ]; then
    if rm -rf "$install" 2>/dev/null; then rb_rm=ok; else rb_rm=failed; fi
    log_event rollback removed-failed-bundle info "path=$install" "result=$rb_rm"
    if mv "$backup" "$install" 2>/dev/null; then rb_mv=ok; else rb_mv=failed; fi
    rb_restored=$(plist_string "$install" CFBundleShortVersionString 2>/dev/null || printf '')
    log_event rollback restored-backup info "from=$backup" "to=$install" "result=$rb_mv" "restoredVersion=$rb_restored"
  else
    log_event rollback no-backup error "backup=$backup" "install=$install"
  fi
  [ -n "$rb_restored" ] || rb_restored="$from"

  if [ -n "$marker" ] && [ -n "$version" ]; then
    rb_oneline=$(printf '%s' "$rb_detail" | tr '\n' ' ')
    {
      printf 'version=%s\n' "$version"
      printf 'reason=%s\n' "$rb_reason"
      printf 'detail=%s\n' "$rb_oneline"
      printf 'attempt=%s\n' "$attempt"
      printf 'from=%s\n' "$from"
      printf 'restored=%s\n' "$rb_restored"
      printf 'at=%s\n' "$(now_iso)"
    } > "$marker.detail"
    printf '%s\n' "$version" > "$marker"
    log_event rollback marker-written info "marker=$marker" "reason=$rb_reason"
  fi

  rb_capture="${TMPDIR:-/tmp}/boss-relaunch-restore.$$"
  open "$install" >"$rb_capture" 2>&1 </dev/null &
  rb_open_pid=$!
  rb_started=$(now_s)
  while is_alive "$rb_open_pid" && [ "$(( $(now_s) - rb_started ))" -lt "$launch_timeout" ]; do sleep 0.1; done
  is_alive "$rb_open_pid" && kill -KILL "$rb_open_pid" 2>/dev/null
  wait "$rb_open_pid"; rb_rc=$?
  rb_reopen=$(cat "$rb_capture" 2>/dev/null); rm -f "$rb_capture"
  log_event rollback reopened-previous info "path=$install" "openExit=$rb_rc" "openOutput=$rb_reopen" "restoredVersion=$rb_restored"
  log_event rollback completed error "reason=$rb_reason" "restoredVersion=$rb_restored"
  exit 1
}

# ---------------------------------------------------------------------------
# 0. Start
# ---------------------------------------------------------------------------

# Legacy invocation (Boss 1.0.734 and earlier pass no --log/--attempt/--from): derive
# them so the trail is still complete. The first-launch flag lives directly in the
# Updates directory, and the log is Updates/log/update-log.jsonl
# (UpdateLog.directory(forUpdatesDirectory:)). The minted id has the app's
# `upd-<unix-ms>-<hex>` shape; `from` is the version of the bundle being replaced.
legacy_args=""
if [ -z "$logfile" ] && [ -n "$flag" ]; then logfile="$(dirname "$flag")/log/update-log.jsonl"; legacy_args="${legacy_args}log "; fi
if [ -z "$attempt" ]; then attempt="upd-$(date +%s)000-$(printf '%04x' "$(( $$ % 65536 ))")"; legacy_args="${legacy_args}attempt "; fi
if [ -z "$from" ]; then from=$(plist_string "$backup" CFBundleShortVersionString 2>/dev/null || printf 'unknown'); legacy_args="${legacy_args}from "; fi

log_event relaunch helper-started info "oldAppPid=$pid" "install=$install" "backup=$backup" "flag=$flag" \
  "marker=$marker" "watchdogSeconds=$watchdog" "oldExitTimeoutSeconds=$old_exit_timeout" \
  "launchTimeoutSeconds=$launch_timeout" "helperPid=$$" "derivedArguments=${legacy_args:-none}"

# ---------------------------------------------------------------------------
# 1. Wait for the old Boss process to exit so the relaunch is a clean cold start.
# ---------------------------------------------------------------------------

wait_started=$(now_s)
old_exit_step="$old_exit_timeout"
if [ -n "$pid" ]; then
  while is_alive "$pid"; do
    elapsed=$(( $(now_s) - wait_started ))
    if [ "$elapsed" -ge "$old_exit_timeout" ]; then
      # Keep waiting: this helper is the swapped bundle's only first-launch watchdog,
      # so exiting would leave an untried bundle installed with nothing to roll it
      # back, and launching now would only activate the still-running old process.
      log_event relaunch old-process-still-running warning "oldAppPid=$pid" "waitedSeconds=$elapsed" \
        "action=continuing to wait before watched launch"
      old_exit_timeout=$((old_exit_timeout + old_exit_step))
    fi
    sleep 0.1
  done
fi
log_event relaunch old-process-exited info "oldAppPid=$pid" "waitedSeconds=$(( $(now_s) - wait_started ))"

# ---------------------------------------------------------------------------
# 2. Assess the swapped bundle before launching it.
# ---------------------------------------------------------------------------

exe_name=$(executable_name "$install")
exe_path="$install/Contents/MacOS/$exe_name"
installed_version=$(plist_string "$install" CFBundleShortVersionString 2>/dev/null || printf 'unreadable')
if [ -x "$exe_path" ]; then exe_state=executable; elif [ -e "$exe_path" ]; then exe_state=not-executable; else exe_state=missing; fi
if command -v xattr >/dev/null 2>&1; then
  if xattr -p com.apple.quarantine "$install" >/dev/null 2>&1; then quarantine=present; else quarantine=absent; fi
else
  quarantine=unavailable
fi
if command -v codesign >/dev/null 2>&1; then
  if cs_out=$(codesign --verify --deep --strict "$install" 2>&1); then codesign_state=ok; else codesign_state="failed: $cs_out"; fi
else
  codesign_state=unavailable
fi
# Any process already running the installed executable before `open` is not the new
# launch: remember it so the launch loop never adopts (and a rollback never kills) it.
other_pid=$(find_running_pid "$install/Contents/MacOS/" | tr '\n' ' ' | sed 's/ *$//')
log_event relaunch bundle-assessment info "install=$install" "installedVersion=$installed_version" \
  "expectedVersion=$version" "executable=$exe_path" "executableState=$exe_state" \
  "quarantine=$quarantine" "codesign=$codesign_state" "otherRunningPid=${other_pid:-none}"

if [ "$exe_state" != executable ]; then
  rollback bundle-invalid "executable $exe_path is $exe_state"
fi
if [ -n "$other_pid" ]; then
  log_event relaunch preexisting-process warning "pids=$other_pid" \
    "action=excluded from new-process discovery and never terminated by rollback"
fi
if [ "$installed_version" != "$version" ]; then
  log_event relaunch version-mismatch warning "installedVersion=$installed_version" "expectedVersion=$version"
fi

# ---------------------------------------------------------------------------
# 3. Clear any stale flag, then launch and confirm a new process appeared.
# ---------------------------------------------------------------------------

[ -n "$flag" ] && rm -f "$flag"
log_event handshake watch-started info "flag=$flag" "flagVersion=$version" "watchdogSeconds=$watchdog" \
  "launchTimeoutSeconds=$launch_timeout"

# `open` blocks until LaunchServices reports the app launched (observed ~2 s), so it
# runs in the background and the process table is polled concurrently from the
# moment it starts: a new process that crashes within open's own latency would
# otherwise be gone before the first poll and look like it never started.
open_capture="${TMPDIR:-/tmp}/boss-relaunch-open.$$"
open "$install" >"$open_capture" 2>&1 </dev/null &
open_bg=$!
open_rc=""
launch_started=$(now_s)

record_open_result() {
  [ -z "$open_rc" ] || return
  if is_alive "$open_bg"; then
    [ "${1:-poll}" = finish ] || return
    kill -KILL "$open_bg" 2>/dev/null
  fi
  wait "$open_bg"; open_rc=$?
  open_out=$(cat "$open_capture" 2>/dev/null | tr '\n' ' '); rm -f "$open_capture"
  log_event relaunch open info "path=$install" "exit=$open_rc" "output=$open_out" \
    "secondsToReturn=$(( $(now_s) - launch_started ))"
}

while :; do
  new_pid=$(find_running_pid "$install/Contents/MacOS/" | head -1)
  [ -n "$new_pid" ] && { watch_started=$(now_s); break; }
  if [ -z "$open_rc" ] && ! is_alive "$open_bg"; then
    record_open_result
    if [ "$open_rc" -ne 0 ]; then
      rollback open-failed "open exited $open_rc: $open_out"
    fi
  fi
  elapsed=$(( $(now_s) - launch_started ))
  if [ "$elapsed" -ge "$launch_timeout" ]; then
    [ -n "$open_rc" ] || record_open_result finish
    # Record what the process table did show, so "never started" can be told apart
    # from "a Boss from another bundle path is running" or "ps itself failed".
    scan=$(ps -axww -o pid=,command= 2>&1); scan_rc=$?
    scan_matches=$(printf '%s\n' "$scan" | grep -F "$(basename "$install")/Contents/MacOS/" | head -5 | cut -c1-200 | tr '\n' '|')
    log_event relaunch process-scan warning "psExit=$scan_rc" "psLines=$(printf '%s\n' "$scan" | wc -l | tr -d ' ')" \
      "appExecutablesRunning=${scan_matches:-none}" "psError=$( [ "$scan_rc" -ne 0 ] && printf '%s' "$scan" | head -2 | tr '\n' ' ' )"
    rollback launch-not-started "open exited ${open_rc:-?} but no process running $exe_path was observed within ${launch_timeout}s (never started, or exited faster than the 0.1s poll; check DiagnosticReports)"
  fi
  sleep 0.1
done
# The process is up; still collect open's own result for the trail (it returns once
# LaunchServices considers the launch complete, normally within a second or two).
[ -n "$open_rc" ] || record_open_result
# Exit status of the new process: it is LaunchServices' child, not ours, so `wait`
# cannot report it. The bundled exit-observer registers a kqueue EVFILT_PROC
# NOTE_EXIT|NOTE_EXITSTATUS watch and prints `exitCode=N` / `signal=N`. Its kernel
# wait is bounded by the watchdog, plus a margin.
exit_capture="${TMPDIR:-/tmp}/boss-relaunch-exit.$$"
observer=""
for obs_candidate in "$(dirname "$0")/bin/exit-observer" "$(dirname "$0")/exit-observer"; do
  [ -x "$obs_candidate" ] && { observer="$obs_candidate"; break; }
done
observer_pid=""
if [ -n "$observer" ]; then
  "$observer" "$new_pid" "$(( watchdog + 5 ))" >"$exit_capture" 2>&1 </dev/null &
  observer_pid=$!
else
  printf 'observerUnavailable=missing (no exit-observer next to %s)\n' "$0" >"$exit_capture"
fi
stop_observer() {
  if [ -n "$observer_pid" ]; then kill "$observer_pid" 2>/dev/null; wait "$observer_pid" 2>/dev/null; fi
  rm -f "$exit_capture"
}
new_exe=$(ps -p "$new_pid" -o command= 2>/dev/null | head -1)
log_event relaunch new-process-started info "pid=$new_pid" "secondsAfterOpen=$(( $(now_s) - launch_started ))" \
  "command=$new_exe" "version=$installed_version"

# ---------------------------------------------------------------------------
# 4. Watchdog: poll for the first-launch-OK flag, timed from the new process start.
# ---------------------------------------------------------------------------

ok=0
alive=1
while :; do
  record_open_result
  if [ -n "$flag" ] && [ -f "$flag" ]; then ok=1; break; fi
  if ! is_alive "$new_pid"; then
    # Re-check once: the flag may have landed just before the process went away.
    if [ -n "$flag" ] && [ -f "$flag" ]; then ok=1; fi
    alive=0
    break
  fi
  elapsed=$(( $(now_s) - watch_started ))
  [ "$elapsed" -ge "$watchdog" ] && break
  sleep 0.1
done
record_open_result finish
handshake_elapsed=$(( $(now_s) - watch_started ))

if [ "$ok" -eq 1 ]; then
  stop_observer
  log_event handshake flag-observed info "flag=$flag" "secondsAfterProcessStart=$handshake_elapsed" "pid=$new_pid"
  if [ -n "$backup" ]; then
    if rm -rf "$backup" 2>/dev/null; then ok_rm=ok; else ok_rm=failed; fi
    log_event relaunch backup-removed info "backup=$backup" "result=$ok_rm"
  fi
  log_event relaunch committed info "version=$installed_version" "pid=$new_pid"
  exit 0
fi

# ---------------------------------------------------------------------------
# 5. Failure: record why, then roll back.
# ---------------------------------------------------------------------------

if [ "$alive" -eq 0 ]; then
  # The process is gone, so the observer's NOTE_EXIT has fired (or it reported why it
  # could not watch); its wait is bounded by the watchdog either way.
  [ -z "$observer_pid" ] || wait "$observer_pid" 2>/dev/null
  exit_detail=$(cat "$exit_capture" 2>/dev/null | tr '\n' ' ' | sed 's/ *$//')
  rm -f "$exit_capture"
  observer_pid=""
  log_event handshake process-exited error "pid=$new_pid" "secondsAfterProcessStart=$handshake_elapsed" "flag=$flag" \
    "termination=$exit_detail"
  rollback process-exited "pid $new_pid exited ${handshake_elapsed}s after starting without writing $flag; $exit_detail"
fi
stop_observer
log_event handshake timeout error "pid=$new_pid" "watchdogSeconds=$watchdog" "flag=$flag" "processAlive=yes"
rollback handshake-timeout "no first-launch flag $flag within ${watchdog}s of pid $new_pid starting (process still alive)"
