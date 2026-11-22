#!/bin/bash
#
# Guards the wiring between ci-env.sh's bazel_verify_only / verify-only
# tool-install and the `ci-strip` config in .ci.bazelrc: both must select a
# config that the rc file really defines, the config must pass the Rust strip
# flag, and the shared CI_BAZEL_FLAGS assignment (which release builds use)
# must not hard-code it.

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

# bazel_verify_only still selects the config on the command it wraps.
if ! grep -A20 '^bazel_verify_only()' "$env_sh" | grep -q -- 'bazel "$subcommand" --config=ci-strip'; then
  fail "--config=ci-strip is not selected by bazel_verify_only"
fi

# Tool-install gets the same config through bazel-step-flags.sh when the step
# sets CI_BAZEL_VERIFY_ONLY=1. The literal must live there, not in the shared
# CI_BAZEL_FLAGS assignment, so release builds keep their symbols.
flags_sh="${workspace}/.buildkite/steps/bazel-step-flags.sh"
if ! grep -q -- '--config=ci-strip --compilation_mode=fastbuild' "$flags_sh"; then
  fail "bazel-step-flags.sh does not emit --config=ci-strip for verify-only steps"
fi

# A release step must never pick the strip config up through the shared
# CI_BAZEL_FLAGS assignment (the per-step append is gated on VERIFY_ONLY).
if grep -n 'CI_BAZEL_FLAGS=.*ci-strip' "$env_sh" > /dev/null; then
  fail "CI_BAZEL_FLAGS must not include ci-strip: release builds share it"
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
