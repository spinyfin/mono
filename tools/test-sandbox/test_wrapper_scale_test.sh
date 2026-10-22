#!/bin/bash
#
# Scale regression for the `rust_test` sharding wrapper installed by
# third_party/patches/rules_rust-0.70.0-test-wrapper.patch.
#
# test_filter_guard_test.sh proves the wrapper honours TESTBRIDGE_TEST_ONLY and
# the TEST_SHARD_* protocol on a 5-test fixture. This test drives the wrapper
# *template* itself against a fake libtest binary that enumerates tens of
# thousands of tests, and checks two things the small fixture cannot:
#
#   1. The partition is still exact at scale: every enumerated test lands in
#      exactly one shard, in enumeration order, as `index % TEST_TOTAL_SHARDS`.
#   2. The wrapper's own work before it exec's the shard -- enumerate,
#      partition, build argv -- stays cheap. The original wrapper partitioned
#      with a bash `for` loop, which on macOS bash 3.2 cost ~0.2 ms per
#      enumerated test *per shard*; at 20,000 tests that is ~4 s of CPU per
#      shard and ~40 s across 11 shards, before a single test runs. The awk
#      partition does the same work in well under a second in total. The bound
#      below is an order of magnitude above the awk cost so host contention
#      cannot trip it, and an order of magnitude below the loop's.
#
# The fake binary speaks just enough libtest to drive the wrapper: it answers
# `--list --format terse` with "name: test" lines (honouring an optional
# positional substring filter the way libtest does) and otherwise records the
# argv it was exec'd with, which is what the wrapper would have handed to the
# real test binary.

set -euo pipefail

template="${TEST_SRCDIR:?}/${WRAPPER_TEMPLATE:?}"
if [[ ! -f "${template}" ]]; then
  printf 'wrapper template not found at %s\n' "${template}" >&2
  exit 1
fi

work="${TEST_TMPDIR:?}/wrapper-scale"
mkdir -p "${work}"

# --- Fake libtest binary. ----------------------------------------------------
# FAKE_TEST_COUNT names, grouped by prefix so a substring filter selects a
# known subset. The wrapper runs it as ./<name> from the current directory.
fake_name="fake_libtest"
# Written with printf, not a here-document: bash implements a heredoc via a
# temp file, which the macOS test sandbox does not always permit (the same
# "cannot create temp file for here document: Operation not permitted" the
# wrapper itself guards against).
printf '%s\n' \
  '#!/bin/bash' \
  'set -euo pipefail' \
  'if [[ "${1:-}" == "--list" ]]; then' \
  '  filter=""' \
  '  if [[ "${4:-}" == "--" ]]; then' \
  '    filter="${5:-}"' \
  '  fi' \
  '  awk -v n="${FAKE_TEST_COUNT:?}" -v filter="${filter}" '\''BEGIN {' \
  '    for (i = 0; i < n; i++) {' \
  '      name = sprintf("scale::group_%02d::tests::case_%06d", i % 7, i)' \
  '      if (filter == "" || index(name, filter) > 0) {' \
  '        printf "%s: test\n", name' \
  '      }' \
  '    }' \
  '  }'\''' \
  '  exit 0' \
  'fi' \
  'printf '\''%s\n'\'' "$@" >"${FAKE_ARGV_OUT:?}"' \
  'printf '\''test result: ok. %d passed; 0 failed\n'\'' "$(( $# - 1 ))"' \
  >"${work}/${fake_name}"
chmod +x "${work}/${fake_name}"

# --- Instantiate the template exactly as rust_test's expand_template does. ---
wrapper="${work}/wrapper.sh"
sed \
  -e "s|{{TEST_BINARY}}|${fake_name}|g" \
  -e 's|{{LIBTEST_HARNESS}}|1|g' \
  -e 's|{{SHARDING_ENABLED}}|1|g' \
  "${template}" >"${wrapper}"
chmod +x "${wrapper}"
if grep -q '{{' "${wrapper}"; then
  printf 'unsubstituted placeholder left in the wrapper:\n' >&2
  grep -n '{{' "${wrapper}" >&2
  exit 1
fi

# Failures go to fd 3 (a copy of stderr) so the timed block below can capture
# the `time` keyword's report on fd 2 without failure text landing in it.
exec 3>&2
failures=0
fail() {
  printf 'FAIL: %s\n' "$*" >&3
  failures=$((failures + 1))
}

# Usage: run_shard <count> <filter> <shard_index> <total_shards> <argv-out>
# Every Bazel test-protocol variable is set explicitly so this test's own
# invocation cannot leak into the wrapper. Status in $status.
shard_status_file="${work}/shard_status"
run_shard() {
  local count="$1" filter="$2" shard_index="$3" total_shards="$4" argv_out="$5"
  status=0
  rm -f "${shard_status_file}" "${argv_out}"
  (
    cd "${work}"
    export FAKE_TEST_COUNT="${count}" FAKE_ARGV_OUT="${argv_out}"
    export TEST_SHARD_STATUS_FILE="${shard_status_file}"
    export TEST_SHARD_INDEX="${shard_index}" TEST_TOTAL_SHARDS="${total_shards}"
    if [[ -n "${filter}" ]]; then
      export TESTBRIDGE_TEST_ONLY="${filter}"
    else
      unset TESTBRIDGE_TEST_ONLY
    fi
    "./wrapper.sh"
  ) >"${argv_out}.stdout" 2>&1 || status=$?
}

# Expected shard contents, computed independently of the wrapper from the fake
# binary's own enumeration order.
expected_shard() {
  local count="$1" filter="$2" shard_index="$3" total_shards="$4"
  (
    cd "${work}"
    FAKE_TEST_COUNT="${count}" "./${fake_name}" --list --format terse ${filter:+-- "${filter}"}
  ) | sed 's/: test$//' | awk -v total="${total_shards}" -v shard="${shard_index}" '(NR - 1) % total == shard'
}

count=20000
total=11

# --- Case 1: exact partition at scale, and the wrapper's cost before exec. ---
union="${work}/union"
: >"${union}"
TIMEFORMAT='%U %S'
cpu_log="${work}/cpu"
{ time {
  for ((shard = 0; shard < total; shard++)); do
    argv_out="${work}/argv.${shard}"
    run_shard "${count}" "" "${shard}" "${total}" "${argv_out}"
    if [[ ${status} -ne 0 ]]; then
      fail "shard ${shard}/${total} of ${count} tests exited ${status}; expected 0. Output:
$(cat "${argv_out}.stdout")"
      continue
    fi
    if [[ ! -e "${shard_status_file}" ]]; then
      fail "shard ${shard}: wrapper must touch TEST_SHARD_STATUS_FILE to advertise sharding support"
    fi
    if [[ "$(tail -n 1 "${argv_out}")" != "--exact" ]]; then
      fail "shard ${shard}: wrapper must exec the shard's names followed by --exact; argv ended with '$(tail -n 1 "${argv_out}")'"
    fi
    sed '$d' "${argv_out}" >"${argv_out}.names"
    if [[ "$(cat "${argv_out}.names")" != "$(expected_shard "${count}" "" "${shard}" "${total}")" ]]; then
      fail "shard ${shard}: exec'd names are not the index % ${total} == ${shard} slice of the enumeration (got $(wc -l <"${argv_out}.names" | tr -d ' ') names)"
    fi
    cat "${argv_out}.names" >>"${union}"
  done
} ; } 2>"${cpu_log}"

union_count="$(wc -l <"${union}" | tr -d ' ')"
distinct_count="$(sort -u "${union}" | wc -l | tr -d ' ')"
if [[ "${union_count}" != "${count}" || "${distinct_count}" != "${count}" ]]; then
  fail "${total} shards must together run all ${count} tests exactly once (union=${union_count}, distinct=${distinct_count})"
fi

read -r cpu_user cpu_sys < <(tail -n 1 "${cpu_log}")
cpu_total="$(awk -v u="${cpu_user}" -v s="${cpu_sys}" 'BEGIN { printf "%.2f", u + s }')"
cpu_bound=10
printf 'wrapper CPU across %d shards of %d tests (fake binary and verification included): user=%ss sys=%ss total=%ss (bound %ss)\n' \
  "${total}" "${count}" "${cpu_user}" "${cpu_sys}" "${cpu_total}" "${cpu_bound}"
if awk -v t="${cpu_total}" -v b="${cpu_bound}" 'BEGIN { exit !(t > b) }'; then
  fail "wrapper spent ${cpu_total}s of CPU partitioning ${count} tests into ${total} shards; bound is ${cpu_bound}s. The per-test partition cost has regressed (the bash-loop wrapper this test guards against spent ~0.2 ms per test per shard on macOS bash 3.2)."
fi

# --- Case 2: the filter composes with sharding at scale. --------------------
# TESTBRIDGE_TEST_ONLY is applied during enumeration, so the shards partition
# the filtered set: every group_03 test exactly once, nothing else.
filter="group_03"
filtered_union="${work}/filtered_union"
: >"${filtered_union}"
for ((shard = 0; shard < total; shard++)); do
  argv_out="${work}/filtered_argv.${shard}"
  run_shard "${count}" "${filter}" "${shard}" "${total}" "${argv_out}"
  if [[ ${status} -ne 0 ]]; then
    fail "filtered shard ${shard}/${total} exited ${status}; expected 0. Output:
$(cat "${argv_out}.stdout")"
    continue
  fi
  sed '$d' "${argv_out}" >"${argv_out}.names"
  if [[ "$(cat "${argv_out}.names")" != "$(expected_shard "${count}" "${filter}" "${shard}" "${total}")" ]]; then
    fail "filtered shard ${shard}: exec'd names are not the index % ${total} == ${shard} slice of the filtered enumeration"
  fi
  cat "${argv_out}.names" >>"${filtered_union}"
done
expected_filtered="$(expected_shard "${count}" "${filter}" 0 1)"
if [[ "$(sort "${filtered_union}")" != "$(printf '%s\n' "${expected_filtered}" | sort)" ]]; then
  fail "filter ${filter} must compose with sharding: ${total} shards should together run exactly the $(printf '%s\n' "${expected_filtered}" | grep -c .) filtered tests, ran $(grep -c . "${filtered_union}")"
fi

# --- Case 3: an empty enumeration still fails loudly. -----------------------
run_shard 0 "" 0 "${total}" "${work}/empty_argv"
if [[ ${status} -eq 0 ]]; then
  fail "a binary that enumerates zero tests must not report success under sharding. Output:
$(cat "${work}/empty_argv.stdout")"
elif ! grep -q "enumerated zero tests" "${work}/empty_argv.stdout"; then
  fail "an empty enumeration must explain why it failed. Output:
$(cat "${work}/empty_argv.stdout")"
fi

# --- Case 4: fewer tests than shards leaves the high shards legitimately empty.
run_shard 3 "" "$((total - 1))" "${total}" "${work}/sparse_argv"
if [[ ${status} -ne 0 ]]; then
  fail "shard $((total - 1)) of ${total} with only 3 tests should exit 0 without running anything. Output:
$(cat "${work}/sparse_argv.stdout")"
elif [[ -e "${work}/sparse_argv" ]]; then
  fail "shard $((total - 1)) of ${total} with only 3 tests must not exec the test binary; argv:
$(cat "${work}/sparse_argv")"
fi

if [[ ${failures} -ne 0 ]]; then
  printf '%d case(s) failed\n' "${failures}" >&2
  exit 1
fi

printf 'rust_test sharding wrapper partitions %d tests into %d shards exactly, cheaply\n' "${count}" "${total}"
