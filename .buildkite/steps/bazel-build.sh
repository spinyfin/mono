#!/usr/bin/env bash
# bazel-build.sh — bazel build //... (dependency-graph compile guard).
# Catches visibility violations, missing deps, and broken generated files.
# macOS-only Swift targets (//tools/boss/app-macos/...) are excluded here;
# they run on the mac-app-build step on a macos-arm64 agent.
set -euo pipefail

echo "--- [bazel-build] starting"
echo "[bazel-build] bazelisk: $(bazelisk version 2>&1 | head -1)"

bazel build -- //... -//tools/boss/app-macos/...

echo "[bazel-build] ok"
