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
# The boot id is injected so this runs inside the Bazel sandbox, where the
# kernel boot time is unreadable.
dir=$(fresh_dir stale-boot)
mkdir -p "$dir/lock"
{ echo "pid=$$"; echo "boot=1000"; echo "acquired=$(date +%s)"; echo "label=pre-reboot-job"; } > "$dir/lock/owner"
out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" CI_HOST_BAZEL_LOCK_BOOT_ID=2000 run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
  && printf '%s\n' "$out" | grep -q 'reclaimed stale lock (taken before the last reboot' \
  && pass "pre-reboot lock reclaimed" || fail "pre-reboot lock not reclaimed: $out"

# --- a lock with no owner file is reclaimed after the grace period ---------
dir=$(fresh_dir ownerless)
mkdir -p "$dir/lock"
out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" CI_HOST_BAZEL_LOCK_OWNERLESS_GRACE_SECS=0 run_with_deadline 20 bash -c "sleep 1.1; source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
  && printf '%s\n' "$out" | grep -q 'reclaimed stale lock (no owner file after 0s' \
  && pass "ownerless lock reclaimed after the grace period" || fail "ownerless lock not reclaimed: $out"

# --- an ownerless lock inside the grace period is respected ----------------
dir=$(fresh_dir ownerless-fresh)
mkdir -p "$dir/lock"
if CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 3 bash -c "source '$LIB'; host_bazel_lock_acquire" >/dev/null 2>&1; then
  fail "a fresh ownerless lock was reclaimed inside the grace period"
else
  pass "a fresh ownerless lock is respected"
fi
rm -rf "$dir/lock"

# --- a live holder is never evicted for age; the wait names the long hold --
dir=$(fresh_dir long-hold)
mkdir -p "$dir/lock"
{ echo "pid=$$"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(( $(date +%s) - 100 ))"; echo "label=slow-job"; } > "$dir/lock/owner"
out_file="$dir/waiter.out"
if CI_HOST_BAZEL_LOCK_DIR="$dir" CI_HOST_BAZEL_LOCK_LONG_HOLD_SECS=10 run_with_deadline 3 bash -c "source '$LIB'; host_bazel_lock_acquire" >"$out_file" 2>&1; then
  fail "a live holder was evicted for age"
else
  pass "a live holder stays exclusive past the long-hold mark"
fi
grep -q 'over the 10s long-hold mark' "$out_file" \
  && pass "waiter named the long-held lock" || fail "no long-hold note: $(cat "$out_file")"
rm -rf "$dir/lock"

# --- a delayed reclaim cannot delete a freshly re-acquired live lock -------
# Waiter B judged the old lock stale, then stalled. Meanwhile waiter A
# reclaimed it and a new job took the lock. B's reclaim, run now, must
# re-check the current occupant and leave the live lock alone.
dir=$(fresh_dir reclaim-race)
mkdir -p "$dir/lock"
sleep 0 & dead_pid=$!; wait "$dead_pid"
{ echo "pid=$dead_pid"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=old-job"; } > "$dir/lock/owner"
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_stale_reason '$dir/lock' >/dev/null" \
  || fail "setup: old lock should read as stale"
# A takes over: old lock gone, new live lock owned by this (live) process.
rm -rf "$dir/lock"
mkdir -p "$dir/lock"
{ echo "pid=$$"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=new-job"; } > "$dir/lock/owner"
CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_reclaim '$dir/lock'" >/dev/null 2>&1
if [[ -d "$dir/lock" ]] && grep -q '^label=new-job$' "$dir/lock/owner"; then
  pass "delayed reclaim left the freshly acquired lock alone"
else
  fail "delayed reclaim removed or damaged the new live lock"
fi
rm -rf "$dir/lock"

# --- a reclaim defers while another reclaimer holds the flock --------------
# The reclaim section is serialised by a kernel flock. While another process
# holds it, a reclaim must not touch even a stale lock; once it is released
# the reclaim proceeds.
if command -v perl >/dev/null 2>&1; then
  dir=$(fresh_dir reclaim-flock)
  mkdir -p "$dir/lock"
  sleep 0 & dead_pid=$!; wait "$dead_pid"
  { echo "pid=$dead_pid"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=dead-job"; } > "$dir/lock/owner"
  perl -e '$^F = 255; open(my $f, ">>", $ARGV[0]) or die; flock($f, 2) or die; print "locked\n"; STDOUT->flush; sleep 600' "$dir/reclaim.flock" > "$dir/holder.out" &
  flock_holder=$!
  for _ in 1 2 3 4 5 6 7 8 9 10; do grep -q locked "$dir/holder.out" 2>/dev/null && break; sleep 0.3; done
  CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_reclaim '$dir/lock'" >/dev/null 2>&1
  [[ -d "$dir/lock" ]] && pass "reclaim leaves a stale lock alone while the flock is held elsewhere" \
    || fail "reclaim ran without the flock"
  kill "$flock_holder" 2>/dev/null || true; wait "$flock_holder" 2>/dev/null || true
  CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_reclaim '$dir/lock'" >/dev/null 2>&1
  [[ ! -e "$dir/lock" ]] && pass "reclaim proceeds once the flock holder is gone (kernel released it)" \
    || fail "reclaim did not proceed after the flock was released"

  # Concurrent reclaimers of one stale lock: exactly one reclaims it.
  dir=$(fresh_dir reclaim-concurrent)
  mkdir -p "$dir/lock"
  { echo "pid=$dead_pid"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=dead-job"; } > "$dir/lock/owner"
  for i in 1 2 3 4 5 6; do
    CI_HOST_BAZEL_LOCK_DIR="$dir" bash -c "source '$LIB'; host_bazel_lock_reclaim '$dir/lock'" > "$dir/out.$i" 2>&1 &
  done
  wait
  n=$(cat "$dir"/out.* | grep -c 'reclaimed stale lock' || true)
  [[ "$n" == 1 && ! -e "$dir/lock" ]] && pass "concurrent reclaimers reclaim a stale lock exactly once" \
    || fail "concurrent reclaim reported $n reclaims"
fi

# --- a leftover reclaim.flock file with no holder does not wedge the lock --
dir=$(fresh_dir dead-flock-file)
mkdir -p "$dir/lock"
: > "$dir/reclaim.flock"
sleep 0 & dead_pid=$!; wait "$dead_pid"
{ echo "pid=$dead_pid"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=dead-job"; } > "$dir/lock/owner"
CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" >/dev/null 2>&1 \
  && pass "a leftover reclaim.flock file does not wedge acquisition" || fail "leftover reclaim.flock wedged acquisition"

# --- a live pid with a different start time is a recycled pid: stale -------
# Skipped where the sandbox hides process start times.
if [[ -n "$(bash -c "source '$LIB'; host_bazel_lock_pid_start $$")" ]]; then
  dir=$(fresh_dir recycled-pid)
  mkdir -p "$dir/lock"
  { echo "pid=$$"; echo "start=Thu Jan  1 00:00:00 1970"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=recycled-job"; } > "$dir/lock/owner"
  out=$(CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 20 bash -c "source '$LIB'; host_bazel_lock_acquire; host_bazel_lock_release" 2>&1) \
    && printf '%s\n' "$out" | grep -q 'was recycled' \
    && pass "a recycled pid does not keep a dead holder's lock alive" || fail "recycled-pid lock not reclaimed: $out"

  # ...and the real start time of a live holder keeps its lock.
  dir=$(fresh_dir live-start)
  mkdir -p "$dir/lock"
  { echo "pid=$$"; echo "start=$(bash -c "source '$LIB'; host_bazel_lock_pid_start $$")"; echo "boot=$(bash -c "source '$LIB'; host_bazel_lock_boot_id")"; echo "acquired=$(date +%s)"; echo "label=live-job"; } > "$dir/lock/owner"
  if CI_HOST_BAZEL_LOCK_DIR="$dir" run_with_deadline 3 bash -c "source '$LIB'; host_bazel_lock_acquire" >/dev/null 2>&1; then
    fail "a live holder with a matching start time was reclaimed"
  else
    pass "a live holder with a matching start time keeps its lock"
  fi
  rm -rf "$dir/lock"
fi

# --- agent hooks: pre-command takes the lock for the job, pre-exit frees it
HOOK_DIR="$TEST_SRCDIR/${TEST_WORKSPACE:-_main}/.buildkite/host-hooks"
dir=$(fresh_dir hooks)
# The hooks run as children of this bash -c, standing in for the job
# bootstrap process, whose pid must own the lock.
HOST_BAZEL_LOCK_LIB="$LIB" CI_HOST_BAZEL_LOCK_DIR="$dir" BUILDKITE_PIPELINE_SLUG=appoint bash -c "
  bash '$HOOK_DIR/pre-command' || exit 30
  [[ \"\$(grep -m1 '^pid=' '$dir/lock/owner' | cut -d= -f2)\" == \"\$\$\" ]] || exit 31
  bash '$HOOK_DIR/pre-exit' || exit 32
  [[ ! -e '$dir/lock' ]] || exit 33
" && pass "agent hooks hold the lock for the bootstrap pid and release it" || fail "agent hooks (exit $?)"
dir=$(fresh_dir hooks-skip)
HOST_BAZEL_LOCK_LIB="$LIB" CI_HOST_BAZEL_LOCK_DIR="$dir" BUILDKITE_PIPELINE_SLUG=mono bash -c "
  bash '$HOOK_DIR/pre-command' || exit 40
  [[ ! -e '$dir/lock' ]] || exit 41
" && pass "agent hooks ignore pipelines outside the list" || fail "agent hook took the lock for an unlisted pipeline (exit $?)"

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
