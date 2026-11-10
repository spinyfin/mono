#!/usr/bin/env bash
# Builds and signs the boss-spoke release assets for the platform this agent
# runs on. bin/release owns version resolution, tags, release notes, the draft
# lifecycle, checksums and publication; this script keeps only the build and
# signing work that must run on each OS.
#
# Signing: the Ed25519 private key comes from BOSS_SPOKE_RELEASE_SIGNING_KEY
# (env var, or the Buildkite secrets store). Without it a build phase fails; it
# never uploads an unsigned package. The key is written to an owner-only file
# that is removed on exit, and is never passed on a command line.
set -euo pipefail

RELEASE_LOG_PREFIX="spoke-release"
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

CONFIG="tools/boss/spoke-release.toml"
TAG_PREFIX="boss-spoke-v"
SPOKE_TARGET="//tools/boss/engine/spoke:boss-spoke"
INSTALLER_TARGET="//tools/boss/spoke-package:boss-spoke-package"

# The platform id the package and manifest are named for; it must match
# Platform::host() in //tools/boss/spoke-package.
platform_id() {
  case "$(uname -s)/$(uname -m)" in
    Darwin/arm64) echo "macos-arm64" ;;
    Linux/x86_64) echo "linux-amd64" ;;
    Linux/aarch64 | Linux/arm64) echo "linux-arm64" ;;
    *) die "no spoke package for $(uname -s)/$(uname -m)" ;;
  esac
}

signing_key() {
  if [[ -n "${BOSS_SPOKE_RELEASE_SIGNING_KEY:-}" ]]; then
    printf '%s' "${BOSS_SPOKE_RELEASE_SIGNING_KEY}"
  elif command -v buildkite-agent &>/dev/null; then
    buildkite-agent secret get BOSS_SPOKE_RELEASE_SIGNING_KEY 2>/dev/null || true
  fi
}

phase_prepare() {
  echo "[spoke-release] agent: $(uname -a)"
  local tag
  tag="$(bin/release prepare --config "${CONFIG}")"
  if [[ -z "${tag}" ]]; then
    echo "[spoke-release] prepare skipped — no build phases to upload"
    return
  fi
  if buildkite-agent step get id --step "spoke-release-publish" &>/dev/null; then
    echo "[spoke-release] build phases for ${tag} are already present"
  else
    echo "[spoke-release] prepared ${tag}; uploading build phases"
    buildkite-agent pipeline upload .buildkite/pipeline-spoke-release-builds.yml
  fi
}

phase_build() {
  local tag version platform key work spoke installer out
  release_tag || return
  tag="${RELEASE_TAG}"
  version="${tag#"${TAG_PREFIX}"}"
  platform="$(platform_id)"

  key="$(signing_key)"
  [[ -n "${key}" ]] || die "BOSS_SPOKE_RELEASE_SIGNING_KEY is not set; refusing to build an unsigned release"

  bazel build -c opt "${SPOKE_TARGET}" "${INSTALLER_TARGET}"
  spoke="$(binary_path "${SPOKE_TARGET}")"
  installer="$(binary_path "${INSTALLER_TARGET}")"

  work="$(mktemp -d)"
  # Expand the path now: EXIT runs after this function returns, when a
  # function-local `work` is unset. Under `set -u` a delayed expansion would
  # fail and leave the signing key on the agent.
  trap "rm -rf -- $(printf '%q' "${work}")" EXIT
  out="${work}/out"
  (umask 077 && printf '%s\n' "${key}" > "${work}/release.key")

  "${installer}" build \
    --spoke-bin "${spoke}" \
    --release-id "${version}" \
    --platform "${platform}" \
    --url-base "https://github.com/spinyfin/mono/releases/download/${tag}" \
    --out-dir "${out}"
  "${installer}" sign \
    --manifest "${out}/boss-spoke-${platform}.manifest.json" \
    --key "${work}/release.key" \
    --out "${out}/boss-spoke-${platform}.manifest.json.sig"
  "${installer}" sign \
    --manifest "${installer}" \
    --key "${work}/release.key" \
    --out "${out}/boss-spoke-package-${platform}.sig"

  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "boss-spoke-${platform}.tar.gz=${out}/boss-spoke-${platform}.tar.gz" \
    --asset "boss-spoke-${platform}.manifest.json=${out}/boss-spoke-${platform}.manifest.json" \
    --asset "boss-spoke-${platform}.manifest.json.sig=${out}/boss-spoke-${platform}.manifest.json.sig" \
    --asset "boss-spoke-package-${platform}=${installer}" \
    --asset "boss-spoke-package-${platform}.sig=${out}/boss-spoke-package-${platform}.sig"

  rm -rf -- "${work}"
  trap - EXIT
}

phase_publish() {
  local tag
  release_tag || return
  tag="${RELEASE_TAG}"
  bin/release publish --config "${CONFIG}" --tag "${tag}"
}

case "${1:-}" in
  prepare) phase_prepare ;;
  build) phase_build ;;
  publish) phase_publish ;;
  *) die "usage: $0 <prepare|build|publish>" ;;
esac
