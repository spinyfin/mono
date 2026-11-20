#!/bin/bash
#
# Guards the wiring between ci-env.sh's bazel_verify_only and the `ci-strip`
# config in .ci.bazelrc: the function must select a config that the rc file
# really defines, the config must pass the Rust strip flag, and release-style
# `bazel` calls (which must keep their symbols) must not select it.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
rc="${workspace}/.ci.bazelrc"
env_sh="${workspace}/.buildkite/steps/ci-env.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

grep -qxF 'build:ci-strip --@rules_rust//rust/settings:extra_rustc_flag=-Cstrip=symbols' "$rc" \
  || fail ".ci.bazelrc does not define build:ci-strip with -Cstrip=symbols"

# Exactly one place selects the config, and it is bazel_verify_only.
selectors="$(grep -n -- '--config=ci-strip' "$env_sh" | grep -v '^[0-9]*:[[:space:]]*#' || true)"
if [[ "$(printf '%s\n' "$selectors" | grep -c .)" != "1" ]]; then
  fail "expected exactly one non-comment --config=ci-strip in ci-env.sh, got: ${selectors:-none}"
fi
if ! grep -A4 '^bazel_verify_only()' "$env_sh" | grep -q -- '--config=ci-strip'; then
  fail "--config=ci-strip is not selected by bazel_verify_only"
fi

# A release step must never pick the strip config up through CI_BAZEL_FLAGS.
if grep -n 'CI_BAZEL_FLAGS=.*ci-strip' "$env_sh" > /dev/null; then
  fail "CI_BAZEL_FLAGS must not include ci-strip: release builds share it"
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
