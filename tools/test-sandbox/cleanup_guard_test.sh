#!/bin/bash

set -euo pipefail

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
        read -r candidate_marker <"${candidate}/cleanup-child-marker"
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

run_completion_case
run_interruption_case TERM
run_interruption_case HUP
run_interruption_case KILL
