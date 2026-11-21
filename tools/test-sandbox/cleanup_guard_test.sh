#!/bin/bash

set -eEuo pipefail
trap 'printf "cleanup assertion failed at line %s: %s\n" "${LINENO}" "${BASH_COMMAND}" >&2' ERR

if [[ "${OSTYPE:-}" != darwin* ]]; then
  exit 0
fi

wrapper="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/hermetic_test_wrapper"
child="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/cleanup_child"

# Every process of one inner run carries its marker on its command line: the
# wrapper, the watchdog it forks, and the fixture. The fixture's own descendant
# does not, and is checked by pid.
marker_processes() {
  ps -axww -o pid=,command= | grep -F -- "$1" | grep -v -F -- "grep -F" || true
}

assert_no_marker_process() {
  local marker="$1"
  local context="$2"
  local survivors=""
  survivors="$(marker_processes "${marker}")"
  [[ -z "${survivors}" ]] || {
    printf 'processes survived %s:\n%s\n' "${context}" "${survivors}" >&2
    return 1
  }
}

# `signal` is delivered the way the wrapper would really see it. TERM and HUP
# go to the wrapper alone, which must forward them. KILL goes to the wrapper's
# whole process group, as Bazel's test-setup.sh does to an abandoned test
# action: the wrapper gets no chance to run a trap, and the test's own process
# group is not in the group that was killed.
run_interruption_case() {
  local signal="$1"
  local inner_pid=""
  local inner_root=""
  local marker="cleanup-$$-${signal}"

  set -m
  "${wrapper}" "${child}" "${marker}" &
  inner_pid=$!
  set +m

  # Runfiles trees can contend heavily during a fully parallel //... run.
  # Give the inner wrapper enough time to publish its process metadata before
  # concluding that signal forwarding could not be exercised.
  for _ in {1..400}; do
    for candidate in /private/tmp/mono-test.*; do
      if [[ "${candidate}" != "${TEST_TMPDIR}" && -f "${candidate}/cleanup-child-marker" ]]; then
        read -r candidate_marker <"${candidate}/cleanup-child-marker" || continue
        if [[ "${candidate_marker}" == "${marker}" ]]; then
          inner_root="${candidate}"
          break 2
        fi
      fi
    done
    sleep 0.05
  done

  if [[ -z "${inner_root}" ]]; then
    kill -TERM "${inner_pid}" 2>/dev/null || true
    wait "${inner_pid}" 2>/dev/null || true
    printf '%s\n' "inner wrapper did not start" >&2
    return 1
  fi

  read -r child_pid descendant_pid <"${inner_root}/cleanup-child-pids"
  if [[ "${signal}" == "KILL" ]]; then
    kill -KILL -- "-${inner_pid}"
    wait "${inner_pid}" 2>/dev/null || true
    # Nothing is left to wait on: the watchdog that takes over is not a child
    # of this shell. It gives the fixture, which ignores SIGTERM, one second
    # before SIGKILL, then removes the private root.
    for _ in {1..200}; do
      if [[ ! -e "${inner_root}" ]] && ! kill -0 "${child_pid}" 2>/dev/null &&
        ! kill -0 "${descendant_pid}" 2>/dev/null &&
        [[ -z "$(marker_processes "${marker}")" ]]; then
        break
      fi
      sleep 0.05
    done
  else
    kill "-${signal}" "${inner_pid}"
    wait "${inner_pid}" 2>/dev/null || true
  fi

  if [[ "${signal}" == "TERM" || "${signal}" == "KILL" ]]; then
    local pid report
    for pid in "${child_pid}" "${descendant_pid}"; do
      report="${TEST_UNDECLARED_OUTPUTS_DIR:?}/timeout-${child_pid}/threads-${pid}.txt"
      [[ -s "${report}" ]] || { printf 'missing thread backtraces: %s\n' "${report}" >&2; return 1; }
      grep -q 'Call graph:' "${report}"
      grep -q 'Thread_' "${report}"
      grep -q 'stack capture exit=0' "${report}"
    done
  fi

  [[ ! -e "${inner_root}" ]] || {
    printf 'sandbox root survived %s: %s\n' "${signal}" "${inner_root}" >&2
    return 1
  }
  ! kill -0 "${child_pid}" 2>/dev/null || {
    printf 'sandbox child survived %s: %s\n' "${signal}" "${child_pid}" >&2
    return 1
  }
  ! kill -0 "${descendant_pid}" 2>/dev/null || {
    printf 'sandbox descendant survived %s: %s\n' "${signal}" "${descendant_pid}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "${signal}"
}

# A test that ends on its own must leave nothing behind either: the wrapper
# reaps its watchdog before it returns the test's exit status.
run_completion_case() {
  local marker="cleanup-$$-EXIT"
  local status=0

  "${wrapper}" /bin/sh -c 'exit 7' "${marker}" || status=$?
  [[ "${status}" == "7" ]] || {
    printf 'wrapper returned %s for a test that exited 7\n' "${status}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "a normal exit"
}

# Kill the wrapper's group as soon as the watchdog is listening, before the
# test's pid reaches the watchdog, without waiting for the fixture to publish
# its marker. The case asserts that no test process survives.
run_startup_kill_case() {
  local inner_pid=""
  local inner_root=""
  local marker="cleanup-$$-STARTUP"
  local owner=""

  set -m
  "${wrapper}" "${child}" "${marker}" &
  inner_pid=$!
  set +m

  for _ in {1..400}; do
    for candidate in /private/tmp/mono-test.*; do
      if [[ "${candidate}" != "${TEST_TMPDIR}" && -f "${candidate}/watchdog-ready" ]]; then
        # Another run can remove its root or exit while it is being inspected.
        # Keep looking for this run; a vanished discovery candidate is not its owner.
        read -r owner <"${candidate}/watchdog-ready" || continue
        owner_pgid="$(ps -o pgid= -p "${owner}" 2>/dev/null || true)"
        owner_pgid="${owner_pgid// /}"
        [[ -n "${owner_pgid}" ]] || continue
        if [[ "${owner}" == "${inner_pid}" || "${owner_pgid}" == "${inner_pid}" ]]; then
          inner_root="${candidate}"
          break 2
        fi
      fi
    done
    sleep 0.05
  done

  if [[ -z "${inner_root}" ]]; then
    kill -TERM "${inner_pid}" 2>/dev/null || true
    wait "${inner_pid}" 2>/dev/null || true
    printf '%s\n' "watchdog did not become ready" >&2
    return 1
  fi

  kill -KILL -- "-${inner_pid}"
  wait "${inner_pid}" 2>/dev/null || true
  for _ in {1..200}; do
    if [[ ! -e "${inner_root}" ]] && [[ -z "$(marker_processes "${marker}")" ]]; then
      break
    fi
    sleep 0.05
  done

  [[ ! -e "${inner_root}" ]] || {
    printf 'sandbox root survived startup KILL: %s\n' "${inner_root}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "startup KILL"
}

# Park the launcher after it announced its pid but before it created its
# process group, then signal only the wrapper's pid. Until the group exists
# the launcher pid is all there is to signal, so TERM and HUP must reach it,
# and a SIGKILL of the wrapper alone must leave the watchdog to remove it.
run_pre_setpgid_case() {
  local signal="$1"
  local hold="${TEST_TMPDIR}/hold-${signal}"
  local marker="cleanup-$$-PRESETPGID-${signal}"
  local inner_pid=""
  local launcher_pid=""

  : >"${hold}"
  set -m
  MONO_WRAPPER_TEST_HOLD_BEFORE_SETPGID="${hold}" "${wrapper}" "${child}" "${marker}" &
  inner_pid=$!
  set +m

  for _ in {1..400}; do
    [[ -s "${hold}.paused" ]] && break
    sleep 0.05
  done
  [[ -s "${hold}.paused" ]] || {
    kill -KILL -- "-${inner_pid}" 2>/dev/null || true
    wait "${inner_pid}" 2>/dev/null || true
    printf '%s\n' "launcher did not reach the pre-setpgid hold" >&2
    return 1
  }
  read -r launcher_pid <"${hold}.paused"

  kill "-${signal}" "${inner_pid}"
  wait "${inner_pid}" 2>/dev/null || true
  for _ in {1..200}; do
    kill -0 "${launcher_pid}" 2>/dev/null || break
    sleep 0.05
  done
  local survived=0
  ! kill -0 "${launcher_pid}" 2>/dev/null || survived=1
  rm -f "${hold}"
  if [[ "${survived}" == "1" ]]; then
    kill -KILL "${launcher_pid}" 2>/dev/null || true
    printf 'launcher survived %s before setpgid: %s\n' "${signal}" "${launcher_pid}" >&2
    return 1
  fi
  for _ in {1..100}; do
    [[ -z "$(marker_processes "${marker}")" ]] && break
    sleep 0.05
  done
  assert_no_marker_process "${marker}" "${signal} before setpgid"
}

# Hold the watchdog before it publishes readiness, so the wrapper is parked in
# its readiness wait with no launcher started. Prints nothing; sets
# `hold`, `inner_pid`, `inner_root` and `watchdog_pid` for the caller.
start_held_before_ready() {
  local marker="$1"
  local fixture="${2:-}"
  hold="${TEST_TMPDIR}/ready-hold-${marker}"
  inner_root=""
  watchdog_pid=""
  : >"${hold}"
  set -m
  if [[ -n "${fixture}" ]]; then
    MONO_WRAPPER_TEST_HOLD_BEFORE_READY="${hold}" "${wrapper}" /bin/sh -c 'exit 0' "${marker}" &
  else
    MONO_WRAPPER_TEST_HOLD_BEFORE_READY="${hold}" "${wrapper}" "${child}" "${marker}" &
  fi
  inner_pid=$!
  set +m
  for _ in {1..400}; do
    [[ -f "${hold}.paused" && -s "${hold}.watchdog" ]] && break
    sleep 0.05
  done
  [[ -f "${hold}.paused" && -s "${hold}.watchdog" ]] || {
    rm -f "${hold}"
    kill -KILL -- "-${inner_pid}" 2>/dev/null || true
    wait "${inner_pid}" 2>/dev/null || true
    printf '%s\n' "watchdog did not reach the pre-readiness hold" >&2
    return 1
  }
  read -r watchdog_pid inner_root <"${hold}.watchdog"
}

# Waits up to ~10s for `pid` (a child of this shell) to exit; kills its group
# and fails if it does not.
wait_bounded() {
  local pid="$1"
  local what="$2"
  for _ in {1..200}; do
    kill -0 "${pid}" 2>/dev/null || { wait "${pid}" 2>/dev/null || true; return 0; }
    sleep 0.05
  done
  kill -KILL -- "-${pid}" 2>/dev/null || true
  wait "${pid}" 2>/dev/null || true
  printf '%s\n' "wrapper did not exit in bounded time: ${what}" >&2
  return 1
}

# A healthy watchdog that is slow to become ready (longer than the old fixed
# two-second bound) must still end with the test running and succeeding.
run_slow_watchdog_case() {
  local marker="cleanup-$$-SLOWREADY"
  local status=0
  local hold inner_pid inner_root watchdog_pid
  start_held_before_ready "${marker}" "/bin/sh -c 'exit 0'" || return 1
  sleep 3
  rm -f "${hold}"
  wait "${inner_pid}" || status=$?
  [[ "${status}" == "0" ]] || {
    printf 'wrapper failed (%s) after a slow watchdog\n' "${status}" >&2
    return 1
  }
  [[ ! -e "${inner_root}" ]] || {
    printf 'sandbox root survived a slow watchdog: %s\n' "${inner_root}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "a slow watchdog"
}

# A watchdog that dies before readiness: the wrapper must give up in bounded
# time without starting the test, and leave no process or private root.
run_dead_watchdog_case() {
  local marker="cleanup-$$-DEADWATCHDOG"
  local hold inner_pid inner_root watchdog_pid
  start_held_before_ready "${marker}" || return 1
  kill -KILL "${watchdog_pid}"
  wait_bounded "${inner_pid}" "watchdog died before readiness" || return 1
  rm -f "${hold}"
  [[ ! -e "${inner_root}" ]] || {
    printf 'sandbox root survived a dead watchdog: %s\n' "${inner_root}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "a dead watchdog"
}

# TERM/HUP while the wrapper waits for readiness: nothing was launched, so the
# cancellation must be remembered and the test never started.
run_cancel_before_ready_case() {
  local signal="$1"
  local marker="cleanup-$$-CANCEL-${signal}"
  local hold inner_pid inner_root watchdog_pid
  start_held_before_ready "${marker}" || return 1
  kill "-${signal}" "${inner_pid}"
  rm -f "${hold}"
  wait_bounded "${inner_pid}" "${signal} before readiness" || return 1
  for _ in {1..100}; do
    [[ ! -e "${inner_root}" && -z "$(marker_processes "${marker}")" ]] && break
    sleep 0.05
  done
  [[ ! -e "${inner_root}" ]] || {
    printf 'sandbox root survived %s before readiness: %s\n' "${signal}" "${inner_root}" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "${signal} before readiness"
}

# The test must start with SIGINT live, so an INT sent to the wrapper reaches
# a fixture that handles it. A fixture handles INT only if it did not start
# with INT ignored (a shell cannot trap a signal ignored on entry).
run_sigint_forwarding_case() {
  local marker="cleanup-$$-SIGINT"
  local out="${TEST_TMPDIR}/sigint-out"
  local inner_pid=""
  : >"${out}"
  set -m
  "${wrapper}" /bin/sh -c 'trap "echo got-int" INT; trap "" TERM; echo ready; while :; do :; done' "${marker}" >"${out}" 2>&1 &
  inner_pid=$!
  set +m
  for _ in {1..400}; do
    grep -q '^ready$' "${out}" && break
    sleep 0.05
  done
  grep -q '^ready$' "${out}" || {
    printf '%s\n' "fixture did not install its SIGINT handler" >&2
    kill -TERM "${inner_pid}" 2>/dev/null || true
    wait_bounded "${inner_pid}" "SIGINT readiness" || return 1
    return 1
  }
  kill -INT "${inner_pid}"
  wait_bounded "${inner_pid}" "INT forwarding" || return 1
  grep -q got-int "${out}" || {
    printf '%s\n' "the test did not receive SIGINT (started with it ignored?)" >&2
    return 1
  }
  assert_no_marker_process "${marker}" "INT"
}

# A hook path outside the supervisor's private directory must be ignored: the
# wrapper runs the test normally and creates nothing at that path.
run_unbound_hook_case() {
  local marker="cleanup-$$-UNBOUND"
  local outside="${TEST_TMPDIR%/}-outside-hook"
  local inner_pid=""
  : >"${outside}"
  set -m
  MONO_WRAPPER_TEST_HOLD_BEFORE_READY="${outside}" \
    MONO_WRAPPER_TEST_HOLD_BEFORE_SETPGID="${outside}" \
    "${wrapper}" /bin/sh -c 'exit 7' "${marker}" &
  inner_pid=$!
  set +m
  wait_bounded "${inner_pid}" "an unbound hook" || { rm -f "${outside}"; return 1; }
  local paused=0
  [[ ! -e "${outside}.paused" ]] || paused=1
  rm -f "${outside}" "${outside}.paused" "${outside}.watchdog"
  [[ "${paused}" == "0" ]] || {
    printf '%s\n' "an unbound hook path was honoured" >&2
    return 1
  }
}

run_completion_case
run_interruption_case TERM
run_interruption_case HUP
run_interruption_case KILL
run_startup_kill_case
run_pre_setpgid_case TERM
run_pre_setpgid_case HUP
run_pre_setpgid_case KILL
run_slow_watchdog_case
run_dead_watchdog_case
run_cancel_before_ready_case TERM
run_cancel_before_ready_case HUP
run_sigint_forwarding_case
run_unbound_hook_case
