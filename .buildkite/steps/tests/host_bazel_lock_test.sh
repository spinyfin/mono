#!/usr/bin/env bash
# Tests for .buildkite/steps/host-bazel-lock.sh.
#
# Each case runs the library in a fresh bash process (the lock's release
# path is an EXIT trap keyed on the acquiring pid, so the process boundary
# is part of what is under test) against a private lock directory under
# $TEST_TMPDIR, with the lock forced on so the suite is identical on Linux
# and macOS.
set -euo pipefail

LIB="$TEST_SRCDIR/${TEST_WORKSPACE:-_main}/.buildkite/steps/host-bazel-lock.sh"
[[ -r "$LIB" ]] || { echo "missing $LIB"; exit 1; }

# bash here-documents need a writable temp dir; the sandbox's default is not.
export TMPDIR="$TEST_TMPDIR"
export CI_HOST_BAZEL_LOCK=1
export CI_HOST_BAZEL_LOCK_POLL_SECS=1
export CI_HOST_BAZEL_LOCK_REPORT_SECS=1
unset HOST_BAZEL_LOCK_HELD

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }
pass() { echo "ok: $*"; }

# coreutils `timeout` is not on a stock macOS host, so bound a command with
# a bash-native deadline instead. Exit 124 on expiry, like timeout(1).
run_with_deadline() {
  local secs="$1"; shift
  "$@" &
  local pid=$! ticks=0
  while kill -0 "$pid" 2>/dev/null; do
    if (( ticks >= secs * 10 )); then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 0.1
    ticks=$((ticks + 1))
  done
  wait "$pid"
}

fresh_dir() {
  local d="$TEST_TMPDIR/$1"
  rm -rf "$d"
  mkdir -p "$d"
  printf '%s\n' "$d"
}

# --- disabled: CI_HOST_BAZEL_LOCK=0 never touches the filesystem -----------
dir=$(fresh_dir disabled)
CI_HOST_BAZEL_LOCK=0 CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_acquire; [[ -z \"\${HOST_BAZEL_LOCK_HELD:-}\" ]]" \
  && [[ ! -e "$dir/lock" ]] && pass "disabled lock creates nothing" || fail "disabled lock created something or set HELD"

# --- auto mode follows the OS ----------------------------------------------
dir=$(fresh_dir auto)
if [[ "$(uname -s)" == "Darwin" ]]; then
  env -u CI_HOST_BAZEL_LOCK CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_enabled" \
    && pass "auto mode is on for Darwin" || fail "auto mode should be on for Darwin"
else
  env -u CI_HOST_BAZEL_LOCK CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; ! host_bazel_lock_enabled" \
    && pass "auto mode is off for $(uname -s)" || fail "auto mode should be off for $(uname -s)"
fi

# --- acquire writes an owner file naming this pid; release removes it ------
dir=$(fresh_dir basic)
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "
  source '$LIB'
  host_bazel_lock_acquire
  [[ -d '$dir/lock' ]] || exit 10
  [[ \"\$(grep -m1 '^pid=' '$dir/lock/owner' | cut -d= -f2)\" == \"\$\$\" ]] || exit 11
  [[ \"\${HOST_BAZEL_LOCK_HELD:-}\" == 1 ]] || exit 12
  host_bazel_lock_release
  [[ ! -e '$dir/lock' ]] || exit 13
  [[ -z \"\${HOST_BAZEL_LOCK_HELD:-}\" ]] || exit 14
" && pass "acquire/release round trip" || fail "acquire/release round trip (exit $?)"

# --- a second acquire in the same process is a no-op (idempotent) ----------
dir=$(fresh_dir idempotent)
CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 10 bash -c "
  source '$LIB'
  host_bazel_lock_acquire
  host_bazel_lock_acquire
  host_bazel_lock_release
" && pass "re-acquire by the holder does not deadlock" || fail "re-acquire by the holder deadlocked or failed"

# --- the EXIT trap releases when the holding process ends ------------------
dir=$(fresh_dir trap)
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_acquire; exit 0"
[[ ! -e "$dir/lock" ]] && pass "EXIT trap released the lock" || fail "EXIT trap left the lock behind"

# --- a child that inherited HOST_BAZEL_LOCK_HELD must not release ----------
dir=$(fresh_dir child)
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "
  source '$LIB'
  host_bazel_lock_acquire
  bash -c \"source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release\"
  [[ -d '$dir/lock' ]] || exit 20
  host_bazel_lock_release
" && pass "child with inherited HELD neither blocks nor releases" || fail "child released the parent's lock (exit $?)"

# --- a waiter blocks until a live holder releases --------------------------
dir=$(fresh_dir contention)
: > "$dir/events"
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "
  source '$LIB'
  host_bazel_lock_acquire
  echo holder-acquired >> '$dir/events'
  sleep 4
  echo holder-releasing >> '$dir/events'
  host_bazel_lock_release
" &
holder=$!
# Give the holder time to take the lock before the waiter starts.
for _ in 1 2 3 4 5 6 7 8 9 10; do grep -q holder-acquired "$dir/events" 2>/dev/null && break; sleep 0.5; done
waiter_out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 30 bash -c "
  source '$LIB'
  host_bazel_lock_acquire
  echo waiter-acquired >> '$dir/events'
  host_bazel_lock_release
" 2>&1) || fail "waiter did not acquire within 30s"
wait "$holder"
if [[ "$(cat "$dir/events")" == $'holder-acquired\nholder-releasing\nwaiter-acquired' ]]; then
  pass "waiter acquired only after the holder released"
else
  fail "unexpected ordering: $(tr '\n' ' ' < "$dir/events")"
fi
printf '%s\n' "$waiter_out" | grep -q 'waiting: another heavy Bazel job holds this host' \
  && pass "waiter announced what it is waiting on" || fail "waiter did not announce: $waiter_out"
printf '%s\n' "$waiter_out" | grep -Eq 'acquired .* after waiting [1-9][0-9]*s' \
  && pass "waiter reported its wait time" || fail "waiter did not report wait time: $waiter_out"

# --- a lock whose holder pid is gone is reclaimed --------------------------
dir=$(fresh_dir stale-pid)
mkdir -p "$dir/lock"
# Spawn and reap a background process so its pid is certainly dead.
sleep 0 & dead_pid=$!; wait "$dead_pid"
{ echo "pid=$dead_pid"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=dead-job"; } > "$dir/lock/owner"
out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
  && printf '%s\n' "$out" | grep -q 'reclaimed stale lock (holder pid' \
  && pass "dead-pid lock reclaimed" || fail "dead-pid lock not reclaimed: $out"

# --- a lock from before a reboot is reclaimed even if the pid is live ------
dir=$(fresh_dir stale-boot)
if [[ -n "$(bash -c "source '$LIB'; host_bazel_lock_boot_id")" ]]; then
  mkdir -p "$dir/lock"
  { echo "pid=$$"; echo "boot=1"; echo "acquired=$(date +%s)"; echo "label=pre-reboot-job"; } > "$dir/lock/owner"
  out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
    && printf '%s\n' "$out" | grep -q 'reclaimed stale lock (taken before the last reboot' \
    && pass "pre-reboot lock reclaimed" || fail "pre-reboot lock not reclaimed: $out"
else
  echo "skip: no boot id on this platform"
fi

# --- a lock older than the ceiling is reclaimed even if the pid is live ----
dir=$(fresh_dir stale-age)
mkdir -p "$dir/lock"
{ echo "pid=$$"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(( $(date +%s) - 100 ))"; echo "label=wedged-job"; } > "$dir/lock/owner"
out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" CI_HOST_BAZEL_LOCK_MAX_AGE_SECS=10 run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
  && printf '%s\n' "$out" | grep -q 'reclaimed stale lock (older than the 10s ceiling' \
  && pass "over-age lock reclaimed" || fail "over-age lock not reclaimed: $out"

# --- a live, in-date lock held by this very process is NOT reclaimed -------
dir=$(fresh_dir live)
mkdir -p "$dir/lock"
{ echo "pid=$$"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=live-job"; } > "$dir/lock/owner"
if CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 4 bash -c "source '$LIB'; host_bazel_lock_acquire" >/dev/null 2>&1; then
  fail "a live lock was reclaimed"
else
  pass "a live lock is respected (waiter still blocked after 4s)"
fi
rm -rf "$dir/lock"

if (( failures > 0 )); then
  echo "$failures failure(s)"
  exit 1
fi
echo "all host-bazel-lock tests passed"
