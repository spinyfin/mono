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
# Distinct from the Boss app pipeline's `release-tag` meta-data key, so a
# matching spoke draft can be prepared in the same Buildkite build.
export RELEASE_TAG_KEY="${RELEASE_TAG_KEY:-spoke-release-tag}"
RELEASE_SHA_KEY="${RELEASE_SHA_KEY:-spoke-release-sha}"
REQUIRED_CATALOG_PLATFORMS=(macos-arm64 linux-amd64)

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

record_pin_sha() {
  local sha="${1:-}"
  [[ -n "${sha}" ]] || return 0
  export RELEASE_PIN_SHA="${sha}"
  if command -v buildkite-agent &>/dev/null; then
    buildkite-agent meta-data set "${RELEASE_SHA_KEY}" "${sha}"
  fi
}

resolve_pin_sha() {
  if [[ -n "${RELEASE_PIN_SHA:-}" ]]; then
    printf '%s' "${RELEASE_PIN_SHA}"
    return
  fi
  if command -v buildkite-agent &>/dev/null; then
    local recorded
    recorded="$(buildkite-agent meta-data get "${RELEASE_SHA_KEY}" --default '' 2>/dev/null || true)"
    if [[ -n "${recorded}" ]]; then
      printf '%s' "${recorded}"
      return
    fi
  fi
}

checkout_pin_sha() {
  local sha
  sha="$(resolve_pin_sha)"
  [[ -n "${sha}" ]] || die "spoke packages require RELEASE_PIN_SHA (the Boss tag commit)"
  git fetch --tags --prune origin >/dev/null
  git checkout --detach "${sha}"
  echo "[spoke-release] building from ${sha}"
}

phase_prepare() {
  echo "[spoke-release] agent: $(uname -a)"
  # The hub looks up boss-spoke-v{Boss version}. Pin to that version and the
  # matching Boss tag commit so a package labelled 1.0.N is that commit.
  if [[ -z "${RELEASE_PIN_VERSION:-}" ]]; then
    local boss_tag
    boss_tag="$(git tag --list 'boss-v*' --sort=-v:refname | head -1 || true)"
    if [[ -z "${boss_tag}" ]]; then
      echo "[spoke-release] no boss-v tag; nothing to publish"
      return
    fi
    export RELEASE_PIN_VERSION="${boss_tag#"boss-v"}"
    if [[ -z "${RELEASE_PIN_SHA:-}" ]]; then
      RELEASE_PIN_SHA="$(git rev-list -n 1 "${boss_tag}")"
    fi
  elif [[ -z "${RELEASE_PIN_SHA:-}" ]]; then
    RELEASE_PIN_SHA="$(git rev-list -n 1 "boss-v${RELEASE_PIN_VERSION}" 2>/dev/null || true)"
    [[ -n "${RELEASE_PIN_SHA}" ]] || die "no commit for boss-v${RELEASE_PIN_VERSION}"
  fi
  record_pin_sha "${RELEASE_PIN_SHA}"
  checkout_pin_sha
  echo "[spoke-release] pinning spoke packages to Boss ${RELEASE_PIN_VERSION} at ${RELEASE_PIN_SHA}"
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
  checkout_pin_sha

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

  local digest catalog
  digest="$(shasum -a 256 "${out}/boss-spoke-${platform}.manifest.json" | awk '{print $1}')"
  catalog="${out}/spoke-packages.${platform}.json"
  python3 - "${catalog}" "${version}" "${platform}" "${tag}" "${digest}" <<'PY'
import json, sys
path, version, platform, tag, digest = sys.argv[1:]
json.dump(
    {
        "release_id": version,
        "packages": {
            platform: {
                "manifest_url": f"https://github.com/spinyfin/mono/releases/download/{tag}/boss-spoke-{platform}.manifest.json",
                "manifest_sha256": digest,
            }
        },
    },
    open(path, "w"),
    indent=2,
)
print("", file=open(path, "a"))
PY

  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "boss-spoke-${platform}.tar.gz=${out}/boss-spoke-${platform}.tar.gz" \
    --asset "boss-spoke-${platform}.manifest.json=${out}/boss-spoke-${platform}.manifest.json" \
    --asset "boss-spoke-${platform}.manifest.json.sig=${out}/boss-spoke-${platform}.manifest.json.sig" \
    --asset "boss-spoke-package-${platform}=${installer}" \
    --asset "boss-spoke-package-${platform}.sig=${out}/boss-spoke-package-${platform}.sig" \
    --asset "spoke-packages.${platform}.json=${catalog}"

  rm -rf -- "${work}"
  trap - EXIT
}

phase_publish() {
  local tag version work
  release_tag || return
  tag="${RELEASE_TAG}"
  version="${tag#"${TAG_PREFIX}"}"
  work="$(mktemp -d)"
  trap "rm -rf -- $(printf '%q' "${work}")" EXIT
  command -v gh >/dev/null || die "gh is required to merge spoke package catalogs"
  gh release download "${tag}" --repo spinyfin/mono --pattern 'spoke-packages.*.json' --dir "${work}" \
    || die "could not download spoke-packages.*.json fragments for ${tag}"
  python3 - "${work}" "${version}" "${REQUIRED_CATALOG_PLATFORMS[*]}" <<'PY'
import json, pathlib, sys
work, version = pathlib.Path(sys.argv[1]), sys.argv[2]
required = sys.argv[3].split()
packages = {}
for path in sorted(work.glob("spoke-packages.*.json")):
    if path.name == "spoke-packages.json":
        continue
    data = json.loads(path.read_text())
    if data.get("release_id") != version:
        raise SystemExit(f"{path.name} release_id {data.get('release_id')!r} != {version!r}")
    packages.update(data.get("packages") or {})
missing = [platform for platform in required if platform not in packages]
if missing:
    raise SystemExit(f"catalog missing required platforms: {', '.join(missing)}")
catalog = work / "spoke-packages.json"
catalog.write_text(json.dumps({"release_id": version, "packages": packages}, indent=2) + "\n")
print(f"merged {len(packages)} platform(s) into spoke-packages.json")
PY
  [[ -f "${work}/spoke-packages.json" ]] || die "spoke-packages.json was not produced"
  bin/release upload --config "${CONFIG}" --tag "${tag}" \
    --asset "spoke-packages.json=${work}/spoke-packages.json"
  bin/release publish --config "${CONFIG}" --tag "${tag}"
  rm -rf -- "${work}"
  trap - EXIT
}

case "${1:-}" in
  prepare) phase_prepare ;;
  build) phase_build ;;
  publish) phase_publish ;;
  *) die "usage: $0 <prepare|build|publish>" ;;
esac
