#!/usr/bin/env bash
# xcode-env.sh — make Bazel's Apple toolchain autoconfig track the installed Xcode.
#
# SOURCE this (do not exec) before any bazel invocation on a macOS agent:
#   source "$(dirname "${BASH_SOURCE[0]}")/xcode-env.sh"
#
# Why this exists
# ---------------
# The macOS targets resolve their C/Swift toolchain through the auto-detected
# repos @local_config_apple_cc (apple_support) and @local_config_xcode
# (bazel_tools). Each is fetched once and cached in the output base keyed on its
# declared `environ`. Both rules list XCODE_VERSION and DEVELOPER_DIR in that
# `environ`; XCODE_VERSION is a pure cache-busting knob — its value is never
# parsed, only diffed, to "force re-computing the toolchain" (apple_support's
# own comment). The detector reads the live Xcode via xcrun/xcode-select.
#
# When an agent's Xcode/OS is upgraded in place, DEVELOPER_DIR usually stays
# "/Applications/Xcode.app/Contents/Developer" and XCODE_VERSION was never set,
# so Bazel sees NO change to either repo's inputs and silently reuses the
# pre-upgrade SDK. The stale toolchain then resolves to "empty supported
# platforms", BossTests.xctest is never produced, and `bazel test` dies with
# "** TEST EXECUTE FAILED **" (Buildkite build 904, 2026-05-28).
#
# Binding XCODE_VERSION to the live Xcode build id makes the cache key change on
# every Xcode bump, so Bazel re-detects automatically — a durable replacement
# for remembering to `bazel clean --expunge` after each upgrade.
if ! command -v xcodebuild >/dev/null 2>&1; then
  echo "[xcode-env] xcodebuild not found; skipping Apple toolchain cache-key pin" >&2
else
  DEVELOPER_DIR="${DEVELOPER_DIR:-$(xcode-select -p)}"
  export DEVELOPER_DIR
  # "Xcode 26.5\nBuild version 17F42" -> "26.5-17F42" (unique per Xcode build).
  XCODE_VERSION="$(xcodebuild -version | awk 'NR==1 {v=$2} /Build version/ {b=$3} END {print v"-"b}')"
  export XCODE_VERSION
  echo "[xcode-env] DEVELOPER_DIR=${DEVELOPER_DIR}"
  echo "[xcode-env] XCODE_VERSION=${XCODE_VERSION} (Bazel apple_cc/xcode autoconfig cache key)"
fi
