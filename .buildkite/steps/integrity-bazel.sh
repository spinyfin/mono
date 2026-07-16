#!/usr/bin/env bash
# integrity-bazel.sh — full-repo bazel build + test on macOS.
#
# Runs bazel build //... followed by bazel test //... on a macos-arm64 agent.
# Unlike the PR pipeline's bazel-build/bazel-test steps (Linux, Swift excluded),
# this step runs on macOS and covers the full //... target set — including
# //tools/boss/app-macos/... and //tools/boss/installer/... that require the
# Swift/macOS toolchain.
#
# GhosttyKit stub: rules_swift_package_manager runs `swift package describe`
# during Bazel analysis.  A stub xcframework satisfies the SPM manifest parse
# without requiring a real GhosttyKit build.  Same setup as mac-app-build.sh
# and boss-release.sh.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [integrity-bazel] verifying"
echo "[integrity-bazel] agent: $(uname -a)"

# swift_deps runs `swift package describe` during Bazel analysis, which needs
# a GhosttyKit.xcframework at the gitignored ThirdParty/ path (see the script
# for the full rationale). Materialize a parse-only stub if it's absent.
tools/boss/app-macos/scripts/stub-ghosttykit-xcframework.sh

echo "--- [integrity-bazel] bazel build //..."
bazel build --verbose_failures --keep_going //...

echo "--- [integrity-bazel] bazel test //..."
bazel test --test_output=errors --keep_going //...

echo "[integrity-bazel] ok"
