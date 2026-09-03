#!/usr/bin/env bash
# Builds the source checkout's release CLI for the two supported platforms.
# Release-state work (versioning, drafts, checksums, and publishing) belongs to
# bin/release; this script owns only Buildkite fan-out and product builds.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

CONFIG="tools/release/release.toml"
TARGET="//tools/release:release"

die() { echo "error: $*" >&2; exit 1; }

release_tag() {
  local tag
  tag="$(bin/release tag)"
  if [[ -z "${tag}" ]]; then
    echo "[release-release] no tag from prepare — nothing to build" >&2
    return 1
  fi
  printf '%s\n' "${tag}"
}

binary_path() {
  local target="$1" path
  path="$(bazel cquery -c opt --output=files "${target}" 2>/dev/null | grep '^bazel-out/' | head -1 || true)"
  [[ -n "${path}" && -f "${path}" ]] || die "could not locate Bazel output for ${target}"
  printf '%s\n' "${path}"
}

phase_prepare() {
  echo "[release-release] agent: $(uname -a)"
  bazel test //tools/release:release_lib_test
  local tag
  tag="$(bin/release prepare --config "${CONFIG}")"
  if [[ -z "${tag}" ]]; then
    echo "[release-release] prepare skipped — no build phases to upload"
    return
  fi
  if buildkite-agent step get id --step "release-publish" &>/dev/null; then
    echo "[release-release] build phases for ${tag} are already present"
  else
    echo "[release-release] prepared ${tag}; uploading build phases"
    buildkite-agent pipeline upload .buildkite/pipeline-release-builds.yml
  fi
}

phase_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "linux phase must run on Linux (got $(uname -s))"
  local tag path
  if ! tag="$(release_tag)"; then return; fi
  bazel build -c opt "${TARGET}"
  path="$(binary_path "${TARGET}")"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "release-x86_64-unknown-linux-gnu=${path}"
}

phase_darwin() {
  [[ "$(uname -s)" == "Darwin" ]] || die "darwin phase must run on macOS (got $(uname -s))"
  local tag path
  if ! tag="$(release_tag)"; then return; fi
  bazel build -c opt "${TARGET}"
  path="$(binary_path "${TARGET}")"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "release-aarch64-apple-darwin=${path}"
}

phase_publish() {
  local tag
  if ! tag="$(release_tag)"; then return; fi
  bin/release publish --config "${CONFIG}" --tag "${tag}"
}

case "${1:-}" in
  prepare) phase_prepare ;;
  linux) phase_linux ;;
  darwin) phase_darwin ;;
  publish) phase_publish ;;
  *) die "usage: $0 <prepare|linux|darwin|publish>" ;;
esac
