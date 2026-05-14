#!/usr/bin/env bash
# Workspace-status script for the Boss release build.
#
# Called by Bazel when --workspace_status_command is set (always, not just
# with --stamp). The STABLE_* keys go to stable-status.txt; all others go
# to volatile-status.txt.
#
# BUILD_EMBED_LABEL is a special Bazel key: its value is used by
# apple_bundle_version's build_label_pattern mechanism to stamp
# CFBundleShortVersionString in Boss.app's Info.plist.
set -euo pipefail

SHA=$(jj log --no-graph -r @ -T 'commit_id.short(7)' 2>/dev/null || git rev-parse --short HEAD 2>/dev/null || echo "unknown")
BUILD_TIME=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Goes to stable-status.txt — consumed by build_info_rs genrule and
# by boss_pkg_unsigned to embed the SHA in the .pkg filename.
echo "STABLE_BOSS_GIT_SHA $SHA"
echo "STABLE_BOSS_BUILD_TIME $BUILD_TIME"

# Goes to volatile-status.txt — consumed by apple_bundle_version via
# ctx.info_file to stamp CFBundleShortVersionString in Info.plist.
echo "BUILD_EMBED_LABEL $SHA"
