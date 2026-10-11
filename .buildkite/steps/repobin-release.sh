#!/usr/bin/env bash
# Builds the repobin release assets for the two supported platforms.
# Release-state work (versioning, drafts, checksums, and publishing) belongs to
# bin/release; this script owns only Buildkite fan-out and product builds.
set -euo pipefail

RELEASE_LOG_PREFIX="repobin-release"
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

CONFIG="tools/repobin/release.toml"
TARGET="//tools/repobin:repobin"

# die(), release_tag(), and binary_path() are shared with the other release
# step scripts and live in ci-env.sh (parameterised by RELEASE_LOG_PREFIX).

phase_prepare() {
  echo "[repobin-release] agent: $(uname -a)"
  local tag
  tag="$(bin/release prepare --config "${CONFIG}")"
  if [[ -z "${tag}" ]]; then
    echo "[repobin-release] prepare skipped — no build phases to upload"
    return
  fi
  if buildkite-agent step get id --step "repobin-release-publish" &>/dev/null; then
    echo "[repobin-release] build phases for ${tag} are already present"
  else
    echo "[repobin-release] prepared ${tag}; uploading build phases"
    buildkite-agent pipeline upload .buildkite/pipeline-repobin-release-builds.yml
  fi
}

phase_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "linux phase must run on Linux (got $(uname -s))"
  local tag path
  release_tag || return
  tag="${RELEASE_TAG}"
  bazel build -c opt "${TARGET}"
  path="$(binary_path "${TARGET}")"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "repobin-x86_64-unknown-linux-gnu=${path}"
}

phase_darwin() {
  [[ "$(uname -s)" == "Darwin" ]] || die "darwin phase must run on macOS (got $(uname -s))"
  local tag path
  release_tag || return
  tag="${RELEASE_TAG}"
  bazel build -c opt "${TARGET}"
  path="$(binary_path "${TARGET}")"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "repobin-aarch64-apple-darwin=${path}"
}

phase_publish() {
  local tag
  release_tag || return
  tag="${RELEASE_TAG}"
  bin/release publish --config "${CONFIG}" --tag "${tag}"
}

case "${1:-}" in
  prepare) phase_prepare ;;
  linux) phase_linux ;;
  darwin) phase_darwin ;;
  publish) phase_publish ;;
  *) die "usage: $0 <prepare|linux|darwin|publish>" ;;
esac
