#!/usr/bin/env bash
# crash_watchdog_test.sh — deliberate reproduction of the crash-handler
# livelock, and proof that the watchdog bounds it.
#
# The harness installs a stand-in for Sentry/Breakpad's SIGABRT handler that
# spins exactly the way sentry-native 0.7.8's
# `sentry__block_for_signal_handler` does when the guard is owned by another
# thread, then aborts. Without the watchdog that process runs forever (which
# this test also asserts, so a broken reproduction cannot silently turn the
# real assertion green). With it, the process must die inside the grace period.
set -uo pipefail

HARNESS="${1:?usage: crash_watchdog_test.sh <harness>}"
GRACE_SECONDS=2

# Under `bazel test` the sandbox denies the system temp dir, so capture the
# subprocess output in the test's own scratch space. Falls back to a private
# temp dir when the script is run by hand.
WORKDIR="${TEST_TMPDIR:-$(mktemp -d)}"

# 128 + signal number, the shell's encoding for "killed by signal N".
readonly STATUS_SIGABRT=$((128 + 6))
readonly STATUS_SIGALRM=$((128 + 14))

failures=0

fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# run_mode <mode> <deadline_seconds>
# Sets RUN_STATUS, RUN_ELAPSED_TENTHS, RUN_TIMED_OUT, RUN_STDOUT, RUN_STDERR.
run_mode() {
  local mode=$1 deadline=$2
  RUN_STDOUT="$WORKDIR/$mode.out"
  RUN_STDERR="$WORKDIR/$mode.err"
  : >"$RUN_STDOUT"
  : >"$RUN_STDERR"
  RUN_TIMED_OUT=0
  RUN_STATUS=0

  BOSS_CRASH_WATCHDOG_SECONDS="$GRACE_SECONDS" "$HARNESS" "$mode" \
    >"$RUN_STDOUT" 2>"$RUN_STDERR" &
  local pid=$! tenths=0
  local limit=$((deadline * 10))

  while [ "$tenths" -lt "$limit" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      RUN_STATUS=$?
      RUN_ELAPSED_TENTHS=$tenths
      return 0
    fi
    sleep 0.1
    tenths=$((tenths + 1))
  done

  RUN_TIMED_OUT=1
  RUN_ELAPSED_TENTHS=$limit
  kill -9 "$pid" 2>/dev/null
  wait "$pid" 2>/dev/null
  return 0
}

assert_contains() {
  local file=$1 needle=$2 what=$3
  if ! grep -q -- "$needle" "$file"; then
    fail "$what: expected to find '$needle' in:"
    sed 's/^/    /' "$file" >&2
  fi
}

assert_not_contains() {
  local file=$1 needle=$2 what=$3
  if grep -q -- "$needle" "$file"; then
    fail "$what: did not expect '$needle' in:"
    sed 's/^/    /' "$file" >&2
  fi
}

echo "== spin-unguarded: the injected livelock must actually livelock =="
run_mode spin-unguarded 6
if [ "$RUN_TIMED_OUT" -ne 1 ]; then
  fail "spin-unguarded terminated on its own (status $RUN_STATUS) — the" \
    "reproduction is not reproducing, so the guarded case proves nothing"
fi
assert_contains "$RUN_STDOUT" "prior-handler-entered" "spin-unguarded"

echo "== spin-guarded: the watchdog must terminate it inside the grace =="
run_mode spin-guarded 20
if [ "$RUN_TIMED_OUT" -eq 1 ]; then
  fail "spin-guarded never terminated — the watchdog did not fire"
else
  if [ "$RUN_STATUS" -ne "$STATUS_SIGALRM" ]; then
    fail "spin-guarded exited with status $RUN_STATUS, expected" \
      "$STATUS_SIGALRM (killed by SIGALRM)"
  fi
  # Must be bounded, and must not be so eager that it pre-empts a crash
  # handler that was merely slow.
  if [ "$RUN_ELAPSED_TENTHS" -lt 10 ]; then
    fail "spin-guarded died after ${RUN_ELAPSED_TENTHS}00ms, sooner than the" \
      "${GRACE_SECONDS}s grace — the watchdog is cutting crash reporting short"
  fi
  if [ "$RUN_ELAPSED_TENTHS" -gt 120 ]; then
    fail "spin-guarded took ${RUN_ELAPSED_TENTHS}00ms to die, far past the" \
      "${GRACE_SECONDS}s grace"
  fi
fi
# Crash reporting still ran: the prior (Sentry/Breakpad stand-in) handler was
# entered before the watchdog took over.
assert_contains "$RUN_STDOUT" "prior-handler-entered" "spin-guarded"
assert_contains "$RUN_STDERR" "boss crash-watchdog" "spin-guarded"

echo "== chain-returning: a healthy handler must not wait for the watchdog =="
run_mode chain-returning 10
if [ "$RUN_TIMED_OUT" -eq 1 ]; then
  fail "chain-returning never terminated"
else
  if [ "$RUN_STATUS" -ne "$STATUS_SIGABRT" ]; then
    fail "chain-returning exited with status $RUN_STATUS, expected" \
      "$STATUS_SIGABRT (killed by SIGABRT via the chain's re-raise)"
  fi
  if [ "$RUN_ELAPSED_TENTHS" -ge 10 ]; then
    fail "chain-returning took ${RUN_ELAPSED_TENTHS}00ms; it should die" \
      "immediately on re-raise, well inside the ${GRACE_SECONDS}s grace"
  fi
fi
assert_contains "$RUN_STDOUT" "prior-handler-entered" "chain-returning"
assert_not_contains "$RUN_STDERR" "boss crash-watchdog" "chain-returning"

if [ "$failures" -ne 0 ]; then
  echo "$failures assertion(s) failed" >&2
  exit 1
fi
echo "OK"
