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

# bazel_verify_only must NOT pass --config=ci-strip itself: CI_BAZEL_FLAGS
# already carries it from CI_BAZEL_VERIFY_ONLY=1, and passing it twice would
# duplicate -Cstrip=symbols in extra_rustc_flag (a repeatable string_list) and
# discard the analysis cache.
if sed -n '/^bazel_verify_only()/,/^}/p' "$env_sh" | grep -qE '^[[:space:]]*bazel[[:space:]].*--config=ci-strip'; then
  fail "bazel_verify_only must not pass --config=ci-strip explicitly (already in CI_BAZEL_FLAGS)"
fi

# Compose the bootstrap, repobin, and verify-only main command lines and
# assert that their analysis-affecting flags are identical, with ci-strip
# selected exactly once.
flags_sh="${workspace}/.buildkite/steps/bazel-step-flags.sh"
# shellcheck source=../bazel-step-flags.sh
source "$flags_sh"

extract_analysis_flags() {
  local configs=() comp_mode="" defines=()
  local tok
  for tok in "$@"; do
    case "$tok" in
      --config=*) configs+=("$tok") ;;
      --compilation_mode=*) comp_mode="$tok" ;;
      --define=*) defines+=("$tok") ;;
    esac
  done
  printf '%s\n' "${configs[@]}" "$comp_mode" "${defines[@]}"
}

count_ci_strip() {
  local count=0 tok
  for tok in "$@"; do
    if [[ "$tok" == "--config=ci-strip" ]]; then
      count=$((count + 1))
    fi
  done
  echo "$count"
}

if ! (
  export CI_BAZEL_VERIFY_ONLY=1
  unset CI_BAZEL_STEP_FLAGS
  step_flags="$(ci_resolve_bazel_step_flags)"
  CI_BAZEL_FLAGS="--config=ci-linux --experimental_disk_cache_gc_max_size=100G ${step_flags}"
  REPOBIN_BAZEL_FLAGS="$CI_BAZEL_FLAGS"
  CI_BAZEL_VERIFY_ONLY_FLAGS="--remote_download_minimal"

  # 1. Bootstrap: bazel build $CI_BAZEL_FLAGS //tools/repobin:repobin
  set -f
  # shellcheck disable=SC2206
  bootstrap_args=($CI_BAZEL_FLAGS)

  # 2. Repobin: bazel build --compilation_mode=opt $REPOBIN_BAZEL_FLAGS <targets>
  # shellcheck disable=SC2206
  repobin_tail=($REPOBIN_BAZEL_FLAGS)
  repobin_args=(--compilation_mode=opt "${repobin_tail[@]}")

  # 3. Verify-only main: run the real bazel_verify_only() from ci-env.sh
  captured_main=()
  bazel() {
    shift # subcommand
    # shellcheck disable=SC2206
    local ci_prefix=($CI_BAZEL_FLAGS)
    captured_main=("${ci_prefix[@]}" "$@")
  }
  set +f
  eval "$(sed -n '/^bazel_verify_only()/,/^}/p' "$env_sh")"
  bazel_verify_only test //...

  bootstrap_analysis="$(extract_analysis_flags "${bootstrap_args[@]}")"
  repobin_analysis="$(extract_analysis_flags "${repobin_args[@]}")"
  main_analysis="$(extract_analysis_flags "${captured_main[@]}")"

  if [[ "$bootstrap_analysis" != "$repobin_analysis" ]]; then
    echo "FAIL: bootstrap and repobin analysis flags differ:
bootstrap: ${bootstrap_analysis}
repobin: ${repobin_analysis}" >&2
    exit 1
  fi
  if [[ "$bootstrap_analysis" != "$main_analysis" ]]; then
    echo "FAIL: bootstrap and verify-only main analysis flags differ:
bootstrap: ${bootstrap_analysis}
main: ${main_analysis}" >&2
    exit 1
  fi
  for label_args in "bootstrap:${bootstrap_args[*]}" "repobin:${repobin_args[*]}" "main:${captured_main[*]}"; do
    label="${label_args%%:*}"
    set -f
    # shellcheck disable=SC2206
    arr=(${label_args#*:})
    set +f
    n="$(count_ci_strip "${arr[@]}")"
    if [[ "$n" != "1" ]]; then
      echo "FAIL: ${label} selected --config=ci-strip ${n} time(s), expected 1" >&2
      exit 1
    fi
  done
); then
  failures=$((failures + 1))
fi

# Tool-install gets the same config through bazel-step-flags.sh when the step
# sets CI_BAZEL_VERIFY_ONLY=1. The literal must live there, not in the shared
# CI_BAZEL_FLAGS assignment, so release builds keep their symbols.
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
