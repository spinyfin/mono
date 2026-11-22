#!/bin/bash
#
# Tests for .buildkite/steps/bazel-step-flags.sh, the library ci-env.sh uses
# to pick analysis-affecting flags that tool-install shares with the step's
# main bazel command. Also greps the step scripts that source ci-env.sh so a
# verify-only step cannot forget CI_BAZEL_VERIFY_ONLY=1 and a release step
# cannot forget --compilation_mode=opt.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
flags_sh="${workspace}/.buildkite/steps/bazel-step-flags.sh"
# shellcheck source=../bazel-step-flags.sh
source "$flags_sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# Run the resolver in a subshell so cases cannot leak VERIFY_ONLY / STEP_FLAGS
# into each other. Each optional argument is a VAR=value assignment.
resolve_with() {
  (
    unset CI_BAZEL_VERIFY_ONLY CI_BAZEL_STEP_FLAGS
    local assign
    for assign in "$@"; do
      export "${assign?}"
    done
    # shellcheck source=../bazel-step-flags.sh
    source "$flags_sh"
    ci_resolve_bazel_step_flags
  )
}

expect_resolve() {
  local expected="$1" what="$2"
  shift 2
  local actual
  if ! actual="$(resolve_with "$@")"; then
    fail "${what}: expected success with output '${expected}', command failed"
    return
  fi
  if [[ "$actual" != "$expected" ]]; then
    fail "${what}: expected '${expected}', got '${actual}'"
  fi
}

expect_resolve "--compilation_mode=fastbuild" "default (checks / unspecified)"
expect_resolve "--config=ci-strip --compilation_mode=fastbuild" \
  "CI_BAZEL_VERIFY_ONLY=1 matches bazel_verify_only" \
  "CI_BAZEL_VERIFY_ONLY=1"
expect_resolve "--compilation_mode=opt" \
  "explicit CI_BAZEL_STEP_FLAGS wins" \
  "CI_BAZEL_STEP_FLAGS=--compilation_mode=opt"
expect_resolve "--compilation_mode=opt" \
  "explicit CI_BAZEL_STEP_FLAGS wins over VERIFY_ONLY" \
  "CI_BAZEL_VERIFY_ONLY=1" "CI_BAZEL_STEP_FLAGS=--compilation_mode=opt"

# --- ci-env.sh wiring ---------------------------------------------------------

steps="${workspace}/.buildkite/steps"
env_sh="${steps}/ci-env.sh"

if ! grep -q 'bazel-step-flags.sh' "$env_sh"; then
  fail "ci-env.sh does not source bazel-step-flags.sh"
fi
if ! grep -Fq 'CI_BAZEL_STEP_FLAGS="$(ci_resolve_bazel_step_flags)"' "$env_sh"; then
  fail "ci-env.sh does not assign CI_BAZEL_STEP_FLAGS from ci_resolve_bazel_step_flags"
fi
if ! grep -Fq 'export CI_BAZEL_FLAGS="$CI_BAZEL_FLAGS${CI_BAZEL_STEP_FLAGS:+ ${CI_BAZEL_STEP_FLAGS}}"' "$env_sh"; then
  fail "ci-env.sh does not append CI_BAZEL_STEP_FLAGS to CI_BAZEL_FLAGS"
fi
if ! grep -Fq 'export REPOBIN_BAZEL_FLAGS="$CI_BAZEL_FLAGS"' "$env_sh"; then
  fail "ci-env.sh does not set REPOBIN_BAZEL_FLAGS from the appended CI_BAZEL_FLAGS"
fi

# bazel_verify_only must refuse to run unless the step opted in before source.
if ! grep -A20 '^bazel_verify_only()' "$env_sh" | grep -q 'CI_BAZEL_VERIFY_ONLY'; then
  fail "bazel_verify_only does not require CI_BAZEL_VERIFY_ONLY"
fi

# The first CI_BAZEL_FLAGS assignment (OS config + GC cap + remote) must still
# omit ci-strip; the append from ci_resolve_bazel_step_flags is per-step.
first_flags="$(grep -n '^export CI_BAZEL_FLAGS=' "$env_sh" | head -1 || true)"
if printf '%s\n' "$first_flags" | grep -q 'ci-strip'; then
  fail "first CI_BAZEL_FLAGS assignment must not hard-code ci-strip: ${first_flags}"
fi

assert_before_source() {
  local script="$1" pattern="$2" what="$3"
  local file="${steps}/${script}"
  local assign_line source_line
  assign_line="$(awk -v pat="$pattern" '$0 ~ pat { print NR; exit }' "$file")"
  source_line="$(awk '/source .*ci-env\.sh/ { print NR; exit }' "$file")"
  if [[ -z "$assign_line" || -z "$source_line" ]]; then
    fail "${what}: missing '${pattern}' or ci-env.sh source in ${script}"
    return
  fi
  if (( assign_line >= source_line )); then
    fail "${what}: ${script} sets flags at line ${assign_line} after sourcing ci-env.sh at line ${source_line}"
  fi
}

assert_absent() {
  local script="$1" pattern="$2" what="$3"
  if grep -qE "$pattern" "${steps}/${script}"; then
    fail "${what}: ${script} must not contain '${pattern}'"
  fi
}

for script in bazel-build-test.sh mac-app-build.sh integrity-bazel.sh verify-boss-tag-cache.sh; do
  assert_before_source "$script" '^CI_BAZEL_VERIFY_ONLY=1' \
    "verify-only step ${script} must set CI_BAZEL_VERIFY_ONLY=1 before sourcing"
  assert_absent "$script" '^CI_BAZEL_STEP_FLAGS=' \
    "verify-only step ${script} should rely on VERIFY_ONLY, not an explicit STEP_FLAGS override"
done

for script in checks.sh integrity-checkleft.sh; do
  assert_absent "$script" '^CI_BAZEL_VERIFY_ONLY=1' \
    "checkleft step ${script} must not select ci-strip"
done

for script in boss-release.sh checkleft-release.sh release-release.sh \
  changelog-release.sh spoke-release.sh repobin-release.sh; do
  assert_before_source "$script" '^CI_BAZEL_STEP_FLAGS="--compilation_mode=opt"' \
    "release step ${script} must set opt before sourcing"
  assert_absent "$script" '^CI_BAZEL_VERIFY_ONLY=1' \
    "release step ${script} must not select ci-strip"
done

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
