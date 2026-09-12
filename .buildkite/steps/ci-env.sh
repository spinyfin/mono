#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Per-pipeline bazel daemon idle TTL. Low-immediacy pipelines (checkleft-release,
# integrity) run on a cron/manual cadence where a cold start doesn't matter;
# give them a short TTL so their daemon frees its memory ~15min after the last
# invocation instead of lingering for bazel's multi-hour default. The hot `mono`
# PR pipeline keeps a long TTL so back-to-back PR builds stay warm.
case "${BUILDKITE_PIPELINE_SLUG:-}" in
  mono-checkleft-release | mono-changelog-release | mono-integrity | mono-release)
    BAZEL_MAX_IDLE_SECS=900
    ;;
  *)
    BAZEL_MAX_IDLE_SECS=7200
    ;;
esac

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

# Local cube workspaces get the same bin/ layout from .cube/setup.yaml via
# tools/repobin/shim/install-workspace-shims.sh (symlinks that build repobin
# on first use, since a lease cannot afford a bazel build). Keep the two in
# step: bin/checkleft must mean the same binary there as it does here.
echo "+++ installing repobin tools into bin/"
bazel build //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

# checkleft's npm-provisioned checks (`format/oxc`, `lint/oxc`, …) run
# `npx --yes <package>@<pin>`. Node is not part of the Bazel toolchain;
# hosts are expected to have Node >= 22 on PATH. The bazel-any fleet is
# heterogeneous, and at least one Linux agent (greyarea-1) has neither
# `npx` nor a PATH `oxfmt`, which fails those checks with ENOENT. Call
# `ensure_npx` from steps that run checkleft so a missing host Node is
# filled in with a pinned official tarball instead of failing closed.
CI_NODE_VERSION="24.8.0"

ensure_npx() {
  if command -v npx >/dev/null 2>&1; then
    return 0
  fi
  local extra
  for extra in /usr/local/bin /opt/homebrew/bin; do
    if [[ -x "${extra}/npx" ]]; then
      export PATH="${extra}:${PATH}"
      echo "--- [ci-env] found npx at ${extra}/npx; prepended to PATH"
      return 0
    fi
  done

  local os arch sha
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *)
      echo "error: npx is required for checkleft npm checks but is not on PATH (OS $(uname -s) has no CI Node bootstrap)" >&2
      return 1
      ;;
  esac
  case "$(uname -m)" in
    x86_64) arch=x64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *)
      echo "error: npx is required for checkleft npm checks but is not on PATH (arch $(uname -m) has no CI Node bootstrap)" >&2
      return 1
      ;;
  esac
  case "${os}-${arch}" in
    linux-x64) sha=daf68404b478b4c3616666580d02500a24148c0c8d88648372078c03655ec0f7 ;;
    linux-arm64) sha=5eb16b14af5a5f494ed54770822144e847c744fe590f8df093ad4927cf3dd7fd ;;
    darwin-arm64) sha=d81191a1866760eb918caa976c023036bc1fc7405ea31b148905211522045767 ;;
    darwin-x64) sha=6fd8496b59baa8f86a24e3eb03308b763091716ffc6b6e1094d1a5e5697dd6dd ;;
    *)
      echo "error: no pinned Node tarball for ${os}-${arch}" >&2
      return 1
      ;;
  esac

  local tarball="node-v${CI_NODE_VERSION}-${os}-${arch}.tar.gz"
  local cache_root="${HOME}/.cache/mono-ci-node"
  if [[ -d /mnt/ssd && -w /mnt/ssd ]]; then
    cache_root="/mnt/ssd/mono-ci-node"
  fi
  local prefix="${cache_root}/node-v${CI_NODE_VERSION}-${os}-${arch}"
  if [[ -x "${prefix}/bin/npx" ]]; then
    export PATH="${prefix}/bin:${PATH}"
    echo "--- [ci-env] using cached Node ${CI_NODE_VERSION} at ${prefix}"
    return 0
  fi

  echo "--- [ci-env] npx not on PATH; downloading Node ${CI_NODE_VERSION} (${os}-${arch})"
  local tmp
  tmp=$(mktemp -d)
  # Cleanup even if curl/tar/sha fail; mktemp dirs otherwise leak on `return 1`.
  # shellcheck disable=SC2064
  trap "rm -rf '${tmp}'" RETURN
  curl -fsSL "https://nodejs.org/dist/v${CI_NODE_VERSION}/${tarball}" -o "${tmp}/${tarball}"
  local actual
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "${tmp}/${tarball}" | awk '{print $1}')
  else
    actual=$(shasum -a 256 "${tmp}/${tarball}" | awk '{print $1}')
  fi
  if [[ "${actual}" != "${sha}" ]]; then
    echo "error: Node tarball sha256 mismatch (got ${actual}, expected ${sha})" >&2
    return 1
  fi
  tar -xzf "${tmp}/${tarball}" -C "${tmp}"
  mkdir -p "${cache_root}"
  rm -rf "${prefix}"
  mv "${tmp}/node-v${CI_NODE_VERSION}-${os}-${arch}" "${prefix}"
  export PATH="${prefix}/bin:${PATH}"
  command -v npx >/dev/null 2>&1
}
