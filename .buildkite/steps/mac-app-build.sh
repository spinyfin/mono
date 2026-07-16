#!/usr/bin/env bash
# mac-app-build.sh — build and test macOS Swift targets on a macos-arm64 agent.
# Linux agents have no Swift toolchain; this step runs on Zakalwe-1 instead.
# Also builds the installer/pkg targets whose boss_pkg_payload rule transitively
# depends on //tools/boss/app-macos:Boss and therefore requires macOS.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [mac-app-build] building"

# rules_swift_package_manager's swift_deps module extension runs
# `swift package describe` during every Bazel analysis, which needs a
# GhosttyKit.xcframework at the gitignored ThirdParty/ path (see the script
# for the full rationale). Materialize a parse-only stub if it's absent.
tools/boss/app-macos/scripts/stub-ghosttykit-xcframework.sh

bazel build //tools/boss/app-macos/... //tools/boss/installer/...
# Run every macOS Swift test target, not just BossTests, so the UpdateCore
# module's tests (UpdateChecker / UpdateDownloader — the self-update download,
# verification, quarantine-strip, and staging logic) gate merges too. The `...`
# wildcard picks up both //tools/boss/app-macos:BossTests and
# //tools/boss/app-macos/Tests/UpdateCore:UpdateTests.
bazel test --test_output=errors //tools/boss/app-macos/...

echo "[mac-app-build] ok"
