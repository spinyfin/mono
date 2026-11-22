#!/usr/bin/env bash
# bazel-build-test.sh — bazel build //... then bazel test //... in one step.
# Combines the former bazel-build and bazel-test steps so the test phase
# reuses the build phase's local bazel outputs instead of re-analyzing and
# rebuilding on a different agent (the bazel-any queue mixes darwin/linux
# hosts, so cross-step reuse wasn't guaranteed). bazel test already builds
# what it needs, but running an explicit build first keeps a plain build
# breakage attributable on its own, before any test ever runs.
# macOS-only Swift targets (//tools/boss/app-macos/...) and the macOS
# installer package (//tools/boss/installer/...) are excluded here; they run
# on the mac-app-build step on a macos-arm64 agent.
# //tools/boss/installer/... is excluded because boss_pkg_payload transitively
# depends on //tools/boss/app-macos:Boss (Swift), which has no Linux toolchain.
set -euo pipefail

# This step may execute its bazel actions on remote workers; ci-env.sh
# decides whether it does (see remote-execution.sh). It qualifies because
# nothing it passes to bazel is a credential and its build and
# test commands are run only for their result.
CI_REMOTE_EXECUTION_ALLOWED=1
CI_BAZEL_VERIFY_ONLY=1
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [bazel-build] building"

if ! bazel_verify_only build --verbose_failures --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...; then
  echo "^^^ +++"
  echo "[bazel-build] FAILED"
  exit 1
fi

echo "[bazel-build] ok"

echo "--- [bazel-test] testing"

# Use a fresh directory so an agent checkout cannot report a previous job's BEP.
report_dir=$(mktemp -d "${TMPDIR:-/tmp}/bazel-flaky.XXXXXX")
test_status=0
# Preserve retry evidence even with minimal remote output downloads.
bazel_verify_only test --flaky_test_attempts=2 --remote_download_regex='.*testlogs/.*\.(log|xml)$' --build_event_json_file="${report_dir}/events.json" --nobuild_event_json_file_path_conversion --test_output=errors --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/... || test_status=$?

report_status=0
python3 "$(dirname "${BASH_SOURCE[0]}")/report-flaky-tests.py" "${report_dir}/events.json" "${report_dir}" || report_status=$?
if ((report_status == 0)); then
  rm -rf "$report_dir"
fi
# A reporting error must stay visible, and must never hide a test failure.
if ((test_status != 0)); then
  echo "^^^ +++"
  echo "[bazel-test] FAILED"
  exit 1
fi

if ((report_status != 0)); then
  echo "[bazel-test] flaky reporting FAILED"
  exit "$report_status"
fi
echo "[bazel-test] ok"
