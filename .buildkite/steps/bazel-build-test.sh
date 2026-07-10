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

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [bazel-build] building"
echo "[bazel-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

if ! bazel build --verbose_failures --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...; then
  echo "^^^ +++"
  echo "[bazel-build] FAILED"
  exit 1
fi

echo "[bazel-build] ok"

echo "--- [bazel-test] testing"
echo "[bazel-test] bazelisk: $(bazelisk version 2>&1 | head -1)"

if ! bazel test --test_output=errors --keep_going -- //... -//tools/boss/app-macos/... -//tools/boss/installer/...; then
  echo "^^^ +++"
  echo "[bazel-test] FAILED"
  exit 1
fi

echo "[bazel-test] ok"
