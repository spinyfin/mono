#!/usr/bin/env bash
# Builds checkleft release assets. bin/release owns version resolution, tags,
# release notes, draft lifecycle, checksums, retries, and publication; this
# script keeps only the product-specific build work that must run on each OS.
set -euo pipefail

RELEASE_LOG_PREFIX="checkleft-release"
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

CONFIG="tools/checkleft/release.toml"
CARGO_TOML="tools/checkleft/Cargo.toml"
CARGO_LOCK="Cargo.lock"
NATIVE_TARGET="//tools/checkleft:checkleft"
MUSL_TARGET="//tools/checkleft:checkleft_musl"

# die(), release_tag(), and binary_path() are shared with release-release.sh
# and live in ci-env.sh (parameterised by RELEASE_LOG_PREFIX above) so the two
# release step scripts don't carry byte-identical copies.

# The shared release tool records the computed version but never transforms a
# product's source. checkleft's Bazel and Cargo builds read CARGO_PKG_VERSION
# from this checkout, so each independent build phase stamps its ephemeral CI
# checkout before compiling. The tag remains the only committed release record.
stamp_build_version() {
  local version="$1"
  sed -i.bak -E "s|^version = \".*\"|version = \"${version}\"|" "${CARGO_TOML}"
  rm -f "${CARGO_TOML}.bak"
  sed -i.bak -E "/^name = \"checkleft\"$/{n;s|^version = \".*\"|version = \"${version}\"|;}" "${CARGO_LOCK}"
  rm -f "${CARGO_LOCK}.bak"
  grep -qF "version = \"${version}\"" "${CARGO_TOML}" \
    || die "could not stamp ${CARGO_TOML} with ${version}"
}

phase_prepare() {
  echo "[checkleft-release] agent: $(uname -a)"
  local tag
  tag="$(bin/release prepare --config "${CONFIG}")"
  if [[ -z "${tag}" ]]; then
    echo "[checkleft-release] prepare skipped — no build phases to upload"
    return
  fi
  if buildkite-agent step get id --step "checkleft-release-publish" &>/dev/null; then
    echo "[checkleft-release] build phases for ${tag} are already present"
  else
    echo "[checkleft-release] prepared ${tag}; uploading build phases"
    buildkite-agent pipeline upload .buildkite/pipeline-checkleft-release-builds.yml
  fi
}

phase_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "linux phase must run on Linux (got $(uname -s))"
  local tag version path
  if ! tag="$(release_tag)"; then return; fi
  version="${tag#checkleft-v}"
  stamp_build_version "${version}"
  bazel build -c opt "${NATIVE_TARGET}"
  path="$(binary_path "${NATIVE_TARGET}")"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "checkleft-x86_64-unknown-linux-gnu=${path}"
}

phase_musl() {
  [[ "$(uname -s)" == "Linux" ]] || die "musl phase must run on Linux (got $(uname -s))"
  local tag version path reported
  if ! tag="$(release_tag)"; then return; fi
  version="${tag#checkleft-v}"
  stamp_build_version "${version}"
  bazel build -c opt "${MUSL_TARGET}"
  path="$(binary_path "${MUSL_TARGET}")"
  reported="$("${path}" --version 2>&1 | awk '{print $2}')" \
    || die "musl version check could not execute the binary"
  [[ "${reported}" == "${version}" ]] \
    || die "musl version guard failed: binary reports ${reported}, expected ${version}"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "checkleft-x86_64-unknown-linux-musl=${path}"
}

phase_darwin() {
  [[ "$(uname -s)" == "Darwin" ]] || die "darwin phase must run on macOS (got $(uname -s))"
  local tag version arm_path x86_path
  if ! tag="$(release_tag)"; then return; fi
  version="${tag#checkleft-v}"
  stamp_build_version "${version}"
  bazel build -c opt "${NATIVE_TARGET}"
  arm_path="$(binary_path "${NATIVE_TARGET}")"
  x86_path="target/x86_64-apple-darwin/release/checkleft"
  if rustup target add x86_64-apple-darwin \
      && cargo build --release --locked -p checkleft --target x86_64-apple-darwin \
      && [[ -f "${x86_path}" ]]; then
    bin/release upload --config "${CONFIG}" --tag "${tag}" \
      --asset "checkleft-aarch64-apple-darwin=${arm_path}" \
      --asset "checkleft-x86_64-apple-darwin=${x86_path}"
  else
    echo "[checkleft-release] warning: darwin x86_64 build failed; shipping arm64 only"
    bin/release upload --config "${CONFIG}" --tag "${tag}" \
      --asset "checkleft-aarch64-apple-darwin=${arm_path}"
  fi
}

phase_publish() {
  local tag
  if ! tag="$(release_tag)"; then return; fi
  bin/release publish --config "${CONFIG}" --tag "${tag}"
}

case "${1:-}" in
  prepare) phase_prepare ;;
  linux) phase_linux ;;
  musl) phase_musl ;;
  darwin) phase_darwin ;;
  publish) phase_publish ;;
  *) die "usage: $0 <prepare|linux|musl|darwin|publish>" ;;
esac
