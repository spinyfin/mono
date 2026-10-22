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

  # Partition the Darwin disk cache by the exact Swift compiler build so a
  # `.swiftmodule` produced by one swiftc is never served as a cache hit to a
  # build running a different swiftc. This is the durable fix for the recurring,
  # intermittent UpdateCore skew:
  #
  #   error: compiled module was created by a different version of the compiler
  #   '6.3.2.1.108'; rebuild 'UpdateCore' and try again: .../UpdateCore.swiftmodule
  #
  # `.swiftmodule` compatibility is keyed on the swiftlang build id (e.g.
  # swiftlang-6.3.3.1.3) — exactly the token in that error. The `bazel clean
  # --expunge` above (and the retry below) only clear the OUTPUT BASE; they do
  # NOT touch --disk_cache, which lives on /Volumes/ssd and persists across
  # Xcode upgrades. So a disk cache populated by compiler X keeps handing X's
  # UpdateCore.swiftmodule to a build running compiler Y, which rejects it at
  # import time. Heterogeneous Mac agents sharing/seeding one disk cache hit the
  # same skew. Folding the swiftlang build id into the cache path gives each
  # compiler its own directory, so cross-version reuse is impossible by
  # construction — in every cache topology (per-agent upgrade or shared cache).
  SWIFT_BUILD_ID=$(xcrun swiftc --version 2>/dev/null \
    | sed -n 's/.*(\(swiftlang-[0-9][0-9.]*\).*/\1/p' | head -1)
  if [[ -z "$SWIFT_BUILD_ID" ]]; then
    # Never leave the tag empty: an empty tag would collapse every compiler back
    # onto one shared, poisonable directory. Fall back to the Xcode build id.
    SWIFT_BUILD_ID="xcode-$(printf '%s' "$CURRENT_XCODE_VERSION" | tr -c 'A-Za-z0-9._-' '-')"
  fi
  export BAZEL_DARWIN_DISK_CACHE="/Volumes/ssd/bazel/disk_cache/${SWIFT_BUILD_ID}"
  echo "--- [ci-env] Swift toolchain '${SWIFT_BUILD_ID}'; disk cache → ${BAZEL_DARWIN_DISK_CACHE}"
  # Route repobin's own bazel invocations through the same partitioned cache.
  export REPOBIN_BAZEL_FLAGS="${REPOBIN_BAZEL_FLAGS} --disk_cache=${BAZEL_DARWIN_DISK_CACHE}"
fi

# Wrap bazel and pass in ci configuration.
# Automatically detects Xcode version mismatch errors (caused by a stale output
# base after an Xcode upgrade) and recovers by running `bazel clean --expunge`
# then retrying once.
#
# On Darwin, BAZEL_DARWIN_DISK_CACHE (set above) points --disk_cache at a
# swiftlang-build-id-partitioned directory so cross-compiler `.swiftmodule`
# reuse is impossible. This explicit flag intentionally overrides the base
# --disk_cache from .bazelrc / --config=ci-darwin (last --disk_cache wins).
bazel() {
  local subcommand="$1"
  shift

  local extra_flags=()
  if [[ -n "${BAZEL_DARWIN_DISK_CACHE:-}" ]]; then
    extra_flags+=("--disk_cache=${BAZEL_DARWIN_DISK_CACHE}")
  fi

  local tmplog
  tmplog=$(mktemp)

  if command bazel \
    $BAZEL_STARTUP_FLAGS \
    "$subcommand" \
    --config="ci-${OS_TYPE}" \
    ${extra_flags[@]+"${extra_flags[@]}"} \
    "$@" 2>&1 | tee "$tmplog"; then
    rm -f "$tmplog"
    return 0
  fi

  # Check for Xcode version mismatch (stale output base after Xcode upgrade).
  if grep -qE "xcode-locator.*failed|Xcode version.*is not available" "$tmplog" 2>/dev/null; then
    echo "--- Xcode version mismatch detected; running bazel clean --expunge and retrying"
    command bazel $BAZEL_STARTUP_FLAGS clean --expunge
    rm -f "$tmplog"
    command bazel \
      $BAZEL_STARTUP_FLAGS \
      "$subcommand" \
      --config="ci-${OS_TYPE}" \
      ${extra_flags[@]+"${extra_flags[@]}"} \
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

