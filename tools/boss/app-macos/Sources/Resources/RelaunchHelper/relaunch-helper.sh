#!/bin/sh
# relaunch-helper.sh — relaunch Boss into a freshly-swapped bundle and watchdog its
# first launch, leaving a durable trail of every step.
#
# The bundle swap itself (current bundle -> .bak, staged bundle -> install location)
# is performed *in-process* by UpdateCore's `UpdateInstaller.applySwap` before this
# helper is spawned. This script handles only the parts that cannot run inside the
# Boss process once it has exited:
#
#   1. wait for the old Boss PID to exit (clean cold relaunch). If it never does,
#      give up WITHOUT touching the install: `open` on a bundle whose previous
#      process is still registered with LaunchServices merely activates that old
#      process and launches nothing, so proceeding would always look like a
#      failed launch of the new version and roll back a perfectly good install.
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
# the old app's swap records and the new app's handshake/reconcile records.
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
# How long to wait for the old process to exit before giving up (no rollback).
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
# (POSIX sh has no `local`; every helper function uses a distinct variable prefix so
# nested calls — rollback() logging while holding its own detail — cannot clobber.)
log_event() {
  le_stage="$1"; le_event="$2"; le_level="$3"; shift 3
  [ -n "$logfile" ] || return 0
  le_detail=""
  for le_kv in "$@"; do
    le_k="${le_kv%%=*}"
    le_v="${le_kv#*=}"
    le_detail="${le_detail}${le_detail:+,}\"$(json_escape "$le_k")\":\"$(json_escape "$le_v")\""
  done
  mkdir -p "$(dirname "$logfile")" 2>/dev/null
  printf '{"actor":"helper","attempt":%s,"detail":{%s},"event":"%s","from":%s,"level":"%s","pid":%s,"stage":"%s","to":%s,"ts":"%s"}\n' \
    "$(json_or_null "$attempt")" "$le_detail" "$le_event" "$(json_or_null "$from")" "$le_level" "$$" \
    "$le_stage" "$(json_or_null "$version")" "$(now_iso)" >> "$logfile"
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
  ps -axww -o pid=,command= 2>/dev/null | BOSS_RELAUNCH_MATCH_PREFIX="$1" BOSS_RELAUNCH_OLD_PID="$pid" awk '
    {
      p = $1
      $1 = ""
      cmd = substr($0, 2)
      if (index(cmd, ENVIRON["BOSS_RELAUNCH_MATCH_PREFIX"]) > 0 && p != ENVIRON["BOSS_RELAUNCH_OLD_PID"]) { print p; exit }
    }'
}

is_alive() { kill -0 "$1" 2>/dev/null; }

# ---------------------------------------------------------------------------
# Rollback
# ---------------------------------------------------------------------------

new_pid=""

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
    } > "$marker"
    log_event rollback marker-written info "marker=$marker" "reason=$rb_reason"
  fi

  rb_reopen=$(open "$install" 2>&1); rb_rc=$?
  log_event rollback reopened-previous info "path=$install" "openExit=$rb_rc" "openOutput=$rb_reopen" "restoredVersion=$rb_restored"
  log_event rollback completed error "reason=$rb_reason" "restoredVersion=$rb_restored"
  exit 1
}

# ---------------------------------------------------------------------------
# 0. Start
# ---------------------------------------------------------------------------

log_event relaunch helper-started info "oldAppPid=$pid" "install=$install" "backup=$backup" "flag=$flag" \
  "marker=$marker" "watchdogSeconds=$watchdog" "oldExitTimeoutSeconds=$old_exit_timeout" \
  "launchTimeoutSeconds=$launch_timeout" "helperPid=$$"

# ---------------------------------------------------------------------------
# 1. Wait for the old Boss process to exit so the relaunch is a clean cold start.
# ---------------------------------------------------------------------------

wait_started=$(now_s)
if [ -n "$pid" ]; then
  while is_alive "$pid"; do
    elapsed=$(( $(now_s) - wait_started ))
    if [ "$elapsed" -ge "$old_exit_timeout" ]; then
      # Do NOT `open` here: LaunchServices would just activate the still-running old
      # process (verified), and the watchdog would then roll back a good install.
      # The swap stays committed; the new version runs on the next manual launch,
      # where reconcileAfterLaunch sees the pending record and completes it.
      log_event relaunch old-process-still-running error "oldAppPid=$pid" "waitedSeconds=$elapsed" \
        "action=giving up without relaunch or rollback; install left in place"
      exit 2
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
other_pid=$(find_running_pid "$install/Contents/MacOS/")
log_event relaunch bundle-assessment info "install=$install" "installedVersion=$installed_version" \
  "expectedVersion=$version" "executable=$exe_path" "executableState=$exe_state" \
  "quarantine=$quarantine" "codesign=$codesign_state" "otherRunningPid=${other_pid:-none}"

if [ "$exe_state" != executable ]; then
  rollback bundle-invalid "executable $exe_path is $exe_state"
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
  wait "$open_bg"; open_rc=$?
  open_out=$(cat "$open_capture" 2>/dev/null | tr '\n' ' '); rm -f "$open_capture"
  log_event relaunch open info "path=$install" "exit=$open_rc" "output=$open_out" \
    "secondsToReturn=$(( $(now_s) - launch_started ))"
}

while :; do
  new_pid=$(find_running_pid "$install/Contents/MacOS/")
  [ -n "$new_pid" ] && break
  if [ -z "$open_rc" ] && ! is_alive "$open_bg"; then
    record_open_result
    if [ "$open_rc" -ne 0 ]; then
      rollback open-failed "open exited $open_rc: $open_out"
    fi
  fi
  elapsed=$(( $(now_s) - launch_started ))
  if [ "$elapsed" -ge "$launch_timeout" ]; then
    [ -n "$open_rc" ] || record_open_result
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
new_exe=$(ps -p "$new_pid" -o command= 2>/dev/null | head -1)
log_event relaunch new-process-started info "pid=$new_pid" "secondsAfterOpen=$(( $(now_s) - launch_started ))" \
  "command=$new_exe" "version=$installed_version"

# ---------------------------------------------------------------------------
# 4. Watchdog: poll for the first-launch-OK flag, timed from the new process start.
# ---------------------------------------------------------------------------

watch_started=$(now_s)
ok=0
alive=1
while :; do
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
handshake_elapsed=$(( $(now_s) - watch_started ))

if [ "$ok" -eq 1 ]; then
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
  log_event handshake process-exited error "pid=$new_pid" "secondsAfterProcessStart=$handshake_elapsed" "flag=$flag" \
    "note=exit status unavailable (not a child of this helper); check DiagnosticReports for pid $new_pid"
  rollback process-exited "pid $new_pid exited ${handshake_elapsed}s after starting without writing $flag"
fi
log_event handshake timeout error "pid=$new_pid" "watchdogSeconds=$watchdog" "flag=$flag" "processAlive=yes"
rollback handshake-timeout "no first-launch flag $flag within ${watchdog}s of pid $new_pid starting (process still alive)"
