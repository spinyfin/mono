#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

# Size the disk-cache GC cap to this host's disk. .ci.bazelrc sets
# `build:ci --experimental_disk_cache_gc_max_size=3T` fleet-wide; on any host
# whose cache filesystem is smaller than that, size-based GC never fires and
# the cache fills the disk. A full disk does not take the agent offline -- it
# stays connected and keeps accepting jobs it never runs -- so the cap must fit
# the disk the cache actually lives on. ci_disk_cache_* live in
# disk-cache-gc-cap.sh (tested by //.buildkite/steps/tests). The computed value
# is passed on the command line after --config=ci-<os>, which is where bazel
# lets it override the rc file's value. Nothing here falls back to the 3T
# value: if the cache's filesystem cannot be determined, the step fails.
# shellcheck source=disk-cache-gc-cap.sh
source "$(dirname "${BASH_SOURCE[0]}")/disk-cache-gc-cap.sh"

CI_BAZELRC=".ci.bazelrc"
if ! CI_DISK_CACHE_DIR="$(ci_disk_cache_path "$OS_TYPE" "$CI_BAZELRC")" \
  || ! mkdir -p "$CI_DISK_CACHE_DIR" \
  || ! CI_DISK_CACHE_FS_TOTAL_KB="$(ci_disk_cache_fs_total_kb "$CI_DISK_CACHE_DIR")" \
  || ! CI_DISK_CACHE_GC_MAX_SIZE="$(ci_disk_cache_gc_cap_from_total_kb "$CI_DISK_CACHE_FS_TOTAL_KB")"; then
  echo "^^^ +++"
  echo "[ci-env] error: could not size the Bazel disk-cache GC cap for this host (see above). Refusing to run bazel with .ci.bazelrc's fleet-wide 3T cap: on a smaller disk it never fires and the cache fills the disk." >&2
  exit 1
fi
echo "--- [ci-env] disk cache ${CI_DISK_CACHE_DIR}: filesystem is $((CI_DISK_CACHE_FS_TOTAL_KB / 1048576)) GB, GC cap ${CI_DISK_CACHE_GC_MAX_SIZE} (${CI_DISK_CACHE_GC_PERCENT}%)"

# CI_BAZEL_FLAGS is the single source of truth for the per-command bazel flags
# every CI invocation passes: the OS's CI config plus the per-host disk-cache GC
# cap computed above. The two must travel together -- any invocation that
# selects --config=ci-<os> without the cap puts the fleet-wide 3T cap back in
# force for the GC pass that follows it. The bazel() wrapper below and repobin
# (via REPOBIN_BAZEL_FLAGS) both read from here.
export CI_BAZEL_FLAGS="--config=ci-${OS_TYPE} --experimental_disk_cache_gc_max_size=${CI_DISK_CACHE_GC_MAX_SIZE}"
export REPOBIN_BAZEL_FLAGS="$CI_BAZEL_FLAGS"

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

# Wrap bazel and pass in ci configuration (CI_BAZEL_FLAGS, above).
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
    $CI_BAZEL_FLAGS \
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
      $CI_BAZEL_FLAGS \
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

