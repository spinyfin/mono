#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Bazel daemon idle TTL for every CI pipeline and OS: 30 minutes, so idle
# servers on shared CI hosts (e.g. 16 GB Macs) release their memory. This is a
# startup option set only here (CI), never in an rc file a developer's plain
# `bazel` reads; startup options can't be selected via --config.
BAZEL_MAX_IDLE_SECS=1800

BAZEL_STARTUP_FLAGS="--max_idle_secs=${BAZEL_MAX_IDLE_SECS}"

STARTUP_RC=".ci.${OS_TYPE}.startup.bazelrc"
if [[ -f "$STARTUP_RC" ]]; then
  BAZEL_STARTUP_FLAGS="$BAZEL_STARTUP_FLAGS --bazelrc=$STARTUP_RC"
fi

# CI_BAZEL_STARTUP_FLAGS is the single source of truth for bazel startup
# options in CI. Bazel spins up a brand-new server (and lets the old one
# linger for its full idle TTL) whenever startup options differ from the
# currently running server for the same output_base — so every CI code path
# that shells out to bazel (this script's `bazel()` wrapper below, repobin,
# and checkleft's own `bazel build`/`query` calls for buildifier resolution
# and bazel_aspect invocations) MUST read startup flags from here rather than
# constructing their own, or the workspace ends up running two daemons at
# once (roughly doubling its memory footprint) instead of one.
export CI_BAZEL_STARTUP_FLAGS="$BAZEL_STARTUP_FLAGS"

# On macOS, detect Xcode version changes and expunge the stale Bazel output
# base. The apple_cc_configure module extension caches Xcode paths in the
# output base; if Xcode is updated without a clean, subsequent builds fail
# with "Xcode version X is not available on the host machine".
if [[ "$OS_TYPE" == "darwin" ]]; then
  CURRENT_XCODE_VERSION=$(xcrun xcodebuild -version 2>/dev/null | tr '\n' ' ' | xargs || echo "unknown")
  XCODE_VERSION_FILE="${HOME}/.cache/bazelcache/.xcode_version"
  if [[ -f "$XCODE_VERSION_FILE" ]]; then
    LAST_XCODE_VERSION=$(cat "$XCODE_VERSION_FILE")
    if [[ "$CURRENT_XCODE_VERSION" != "$LAST_XCODE_VERSION" ]]; then
      echo "--- [ci-env] Xcode changed ('$LAST_XCODE_VERSION' → '$CURRENT_XCODE_VERSION'); expunging stale Bazel output base"
      command bazel $BAZEL_STARTUP_FLAGS clean --expunge
    fi
  fi
  mkdir -p "$(dirname "$XCODE_VERSION_FILE")"
  echo "$CURRENT_XCODE_VERSION" > "$XCODE_VERSION_FILE"
fi

# Wrap bazel and pass in ci configuration.
# Automatically detects Xcode version mismatch errors (caused by a stale disk
# cache after an Xcode upgrade) and recovers by running `bazel clean --expunge`
# then retrying once.
bazel() {
  local subcommand="$1"
  shift

  local tmplog
  tmplog=$(mktemp)

  if command bazel \
    $BAZEL_STARTUP_FLAGS \
    "$subcommand" \
    --config="ci-${OS_TYPE}" \
    "$@" 2>&1 | tee "$tmplog"; then
    rm -f "$tmplog"
    return 0
  fi

  # Check for Xcode version mismatch (stale disk cache after Xcode upgrade).
  if grep -qE "xcode-locator.*failed|Xcode version.*is not available" "$tmplog" 2>/dev/null; then
    echo "--- Xcode version mismatch detected in disk cache; running bazel clean --expunge and retrying"
    command bazel $BAZEL_STARTUP_FLAGS clean --expunge
    rm -f "$tmplog"
    command bazel \
      $BAZEL_STARTUP_FLAGS \
      "$subcommand" \
      --config="ci-${OS_TYPE}" \
      "$@"
    return $?
  fi

  rm -f "$tmplog"
  return 1
}

# Shared by the release step scripts (checkleft-release.sh, release-release.sh)
# so they don't each carry byte-identical copies. Each script sets
# RELEASE_LOG_PREFIX to its own name before sourcing this file.
die() { echo "error: $*" >&2; exit 1; }

# Sets the RELEASE_TAG global rather than printing to stdout via a command
# substitution: `die` calls `exit 1`, and a `$(...)` substitution runs in a
# subshell, so a die inside one only exits that subshell — the caller's `if
# ! tag="$(release_tag)"` would see a merely-nonzero assignment and take the
# same branch as the documented "no tag" skip, turning a hard failure into a
# silent no-op. Setting a global keeps die's exit reaching the caller's shell.
release_tag() {
  if ! RELEASE_TAG="$(bin/release tag)"; then
    die "bin/release tag failed"
  fi
  if [[ -z "${RELEASE_TAG}" ]]; then
    echo "[${RELEASE_LOG_PREFIX}] no tag from prepare — nothing to build" >&2
    return 1
  fi
}

binary_path() {
  local target="$1" path
  # The bazel() wrapper above does `2>&1 | tee`, so the `2>/dev/null` here does
  # NOT suppress bazel's INFO/Loading/Analyzing lines — on a cold analysis
  # cache those lines can arrive on stdout before the queried path, so the
  # output must be filtered to bazel-out/ lines before taking the first one.
  path="$(bazel cquery -c opt --output=files "${target}" 2>/dev/null | grep '^bazel-out/' | head -1 || true)"
  [[ -n "${path}" && -f "${path}" ]] || die "could not locate Bazel output for ${target}"
  printf '%s\n' "${path}"
}

echo "+++ installing repobin tools into bin/"
bazel build //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

