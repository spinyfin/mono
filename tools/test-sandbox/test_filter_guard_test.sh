#!/bin/bash
#
# Guard test for the `rust_test` execution wrapper installed by
# third_party/patches/rules_rust-0.70.0-test-wrapper.patch.
#
# Bazel communicates two things to a test binary purely through the
# environment, and a libtest binary can read neither on its own:
#
#   TESTBRIDGE_TEST_ONLY                      <- `bazel test --test_filter=...`
#   TEST_SHARD_INDEX / TEST_TOTAL_SHARDS      <- `shard_count = N`
#
# Stock rules_rust 0.70.0 ignores both, so `--test_filter` is accepted and then
# silently runs the entire suite and reports PASSED. This test drives the
# generated wrapper for :test_filter_fixture directly with those variables set
# and asserts the wrapper honours them -- and, just as importantly, that it
# fails loudly rather than reporting a green run that executed something other
# than what was asked for.
#
# Without the patch, the filter and shard cases below all run the full 5-test
# fixture and this test fails.

set -euo pipefail

runfiles_root="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# The rust_test target's executable is the generated wrapper, not the libtest
# binary itself; that wrapper is what `bazel test` runs and what this test
# needs to exercise.
wrapper_rel="tools/test-sandbox/test_filter_fixture_test_wrapper.sh"

if [[ ! -x "${runfiles_root}/${wrapper_rel}" ]]; then
  printf 'fixture executable not found at %s\n' "${runfiles_root}/${wrapper_rel}" >&2
  exit 1
fi

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

# Run the fixture's wrapper from the runfiles root, which is the working
# directory Bazel gives a test. Every Bazel test-protocol variable is set
# explicitly (or explicitly cleared) so this test's own invocation -- including
# anyone running it under `--test_filter` -- cannot leak into the fixture.
#
# Usage: run_wrapper <filter> <shard_index> <total_shards>
# Empty string means "unset". Captures combined output in $out, status in $status.
run_wrapper() {
  local filter="$1" shard_index="$2" total_shards="$3"

  status=0
  # A subshell keeps each case's environment from leaking into the next, and
  # `unset` is portable where GNU `env --unset` is not.
  out=$(
    cd "${runfiles_root}"
    export TEST_SHARD_STATUS_FILE="${shard_status_file}"

    if [[ -n "${filter}" ]]; then
      export TESTBRIDGE_TEST_ONLY="${filter}"
    else
      unset TESTBRIDGE_TEST_ONLY
    fi

    if [[ -n "${total_shards}" ]]; then
      export TEST_SHARD_INDEX="${shard_index}"
      export TEST_TOTAL_SHARDS="${total_shards}"
    else
      unset TEST_SHARD_INDEX
      unset TEST_TOTAL_SHARDS
    fi

    "./${wrapper_rel}" 2>&1
  ) || status=$?
}

# Extract the "N passed" count libtest prints in its result line.
passed_count() {
  printf '%s\n' "$1" | sed -n 's/^test result: .*[^0-9]\([0-9][0-9]*\) passed.*/\1/p' | tail -1
}

# Names of the tests that actually executed.
ran_tests() {
  printf '%s\n' "$1" | sed -n 's/^test \([A-Za-z0-9_:]*\) \.\.\. ok$/\1/p' | sort
}

shard_status_file="${TEST_TMPDIR:?}/shard_status"

# --- Case 1: no filter, no sharding -> the whole fixture runs. ---------------
rm -f "${shard_status_file}"
run_wrapper "" "" ""
if [[ ${status} -ne 0 ]]; then
  fail "unfiltered run exited ${status}; expected 0. Output:
${out}"
elif [[ "$(passed_count "${out}")" != "5" ]]; then
  fail "unfiltered run should execute all 5 fixture tests, got $(passed_count "${out}"). Output:
${out}"
fi

# --- Case 2: TESTBRIDGE_TEST_ONLY restricts the run. -------------------------
# This is the whole point: before the patch this ran all 5 and reported PASSED.
rm -f "${shard_status_file}"
run_wrapper "alpha" "" ""
if [[ ${status} -ne 0 ]]; then
  fail "filtered run exited ${status}; expected 0. Output:
${out}"
else
  actual="$(ran_tests "${out}")"
  expected=$'alpha_one\nalpha_two'
  if [[ "${actual}" != "${expected}" ]]; then
    fail "TESTBRIDGE_TEST_ONLY=alpha should run exactly alpha_one and alpha_two, ran:
${actual}
Output:
${out}"
  fi
fi

# --- Case 3: a filter matching nothing must fail loudly, never pass. ---------
# A silent green run for a filter that selected zero tests is the exact defect
# this wrapper exists to remove, so it must not be reintroduced as "0 tests ok".
rm -f "${shard_status_file}"
run_wrapper "no_such_test_name_anywhere" "" ""
if [[ ${status} -eq 0 ]]; then
  fail "a filter matching no tests must not report success. Output:
${out}"
elif ! printf '%s\n' "${out}" | grep -q "matched none of the tests"; then
  fail "a filter matching no tests must explain why it failed. Output:
${out}"
fi

# --- Case 4: sharding still partitions the suite. ----------------------------
rm -f "${shard_status_file}"
run_wrapper "" 0 2
shard0_status=${status}
shard0="$(ran_tests "${out}")"
shard0_out="${out}"
if [[ ! -e "${shard_status_file}" ]]; then
  fail "wrapper must touch TEST_SHARD_STATUS_FILE to advertise sharding support"
fi

run_wrapper "" 1 2
shard1_status=${status}
shard1="$(ran_tests "${out}")"
shard1_out="${out}"

if [[ ${shard0_status} -ne 0 || ${shard1_status} -ne 0 ]]; then
  fail "sharded runs exited ${shard0_status}/${shard1_status}; expected 0/0. Output:
${shard0_out}
${shard1_out}"
else
  union="$(printf '%s\n%s\n' "${shard0}" "${shard1}" | grep -c . || true)"
  distinct="$(printf '%s\n%s\n' "${shard0}" "${shard1}" | sort -u | grep -c . || true)"
  if [[ "${union}" != "5" || "${distinct}" != "5" ]]; then
    fail "2 shards must together run all 5 tests exactly once (union=${union}, distinct=${distinct}):
shard 0:
${shard0}
shard 1:
${shard1}"
  fi
fi

# --- Case 5: filter and sharding compose. ------------------------------------
# The filter is applied during enumeration, so the shards partition the
# *filtered* set -- not the whole suite.
rm -f "${shard_status_file}"
run_wrapper "alpha" 0 2
compose0_status=${status}
compose0="$(ran_tests "${out}")"
compose0_out="${out}"

run_wrapper "alpha" 1 2
compose1_status=${status}
compose1="$(ran_tests "${out}")"
compose1_out="${out}"

if [[ ${compose0_status} -ne 0 || ${compose1_status} -ne 0 ]]; then
  fail "filtered sharded runs exited ${compose0_status}/${compose1_status}; expected 0/0. Output:
${compose0_out}
${compose1_out}"
else
  combined="$(printf '%s\n%s\n' "${compose0}" "${compose1}" | sort -u | grep -v '^$' || true)"
  expected=$'alpha_one\nalpha_two'
  if [[ "${combined}" != "${expected}" ]]; then
    fail "filter must compose with sharding: 2 shards of TESTBRIDGE_TEST_ONLY=alpha
should together run exactly alpha_one and alpha_two, ran:
${combined}"
  fi
fi

if [[ ${failures} -ne 0 ]]; then
  printf '%d case(s) failed\n' "${failures}" >&2
  exit 1
fi

printf 'rust_test wrapper honours TESTBRIDGE_TEST_ONLY and the TEST_SHARD_* protocol\n'
