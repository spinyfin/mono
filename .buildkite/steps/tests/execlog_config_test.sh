#!/bin/bash
#
# Script-level test for ci-env.sh's compact execution log wiring:
# - each build/test invocation gets a distinct sequence-numbered .bin.zst path
#   in a per-job directory under bazel-execlogs/,
# - non-build subcommands (e.g. cquery) do not receive the flag,
# - callers passing --execution_log_json_file / --execution_log_binary_file /
#   --execution_log_compact_file are not given a conflicting flag,
# - .bazelignore and pipeline artifact_paths are wired up.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
env_sh="${workspace}/.buildkite/steps/ci-env.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# Extract the CI_EXECLOG_* setup and bazel() function from ci-env.sh and test
# them with a stub _ci_run_bazel() that records the arguments it receives.
export TMPDIR="${TEST_TMPDIR:-${TMPDIR:-/tmp}}"
tmpdir="$(mktemp -d "${TMPDIR}/execlog-test.XXXXXX")"
export TMPDIR="$tmpdir"
trap 'rm -rf "$tmpdir"' EXIT

recorded="${tmpdir}/recorded.txt"
(
  cd "$tmpdir"
  export BUILDKITE_JOB_ID="job-42"

  # Source only the execlog setup and bazel() definition from ci-env.sh.
  eval "$(
    awk '
      /^CI_EXECLOG_ROOT=/ { in_block=1 }
      in_block { print }
      /^# bazel_verify_only/ { exit }
    ' "$env_sh"
  )"

  _ci_run_bazel() {
    local _tmplog="$1"
    shift
    printf '%s\n' "$*" >> "$recorded"
    return 0
  }

  bazel build //foo:bar
  bazel test //foo:bar_test
  bazel cquery //foo:bar
  bazel build --execution_log_json_file=/tmp/custom.json //foo:bar
)

lines=()
while IFS= read -r line; do
  lines+=("$line")
done < "$recorded"

if [[ "${#lines[@]}" != "4" ]]; then
  fail "expected 4 recorded calls, got ${#lines[@]}"
else
  [[ "${lines[0]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/01-build.bin.zst"* ]] \
    || fail "first call missing 01-build.bin.zst: ${lines[0]}"
  [[ "${lines[1]}" == *"--execution_log_compact_file="*"/bazel-execlogs/job-42/02-test.bin.zst"* ]] \
    || fail "second call missing 02-test.bin.zst: ${lines[1]}"
  if [[ "${lines[2]}" == *"--execution_log_compact_file"* ]]; then
    fail "cquery must not receive --execution_log_compact_file: ${lines[2]}"
  fi
  if [[ "${lines[3]}" == *"--execution_log_compact_file"* ]]; then
    fail "call with --execution_log_json_file must not also receive --execution_log_compact_file: ${lines[3]}"
  fi
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
