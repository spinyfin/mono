#!/bin/bash

set -euo pipefail

if [[ "${OSTYPE:-}" != darwin* ]]; then
  exit 0
fi

wrapper="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/hermetic_test_wrapper"
child="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/cleanup_child"

run_interruption_case() {
  local signal="$1"
  local inner_pid=""
  local inner_root=""
  local marker="cleanup-$$-${signal}"

  "${wrapper}" "${child}" "${marker}" &
  inner_pid=$!

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
  kill "-${signal}" "${inner_pid}"
  wait "${inner_pid}" 2>/dev/null || true

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
}

run_interruption_case TERM
run_interruption_case HUP
