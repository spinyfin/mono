#!/usr/bin/env bash
# boss-release.sh — post-merge release step.
#
# Loads shake credentials, stubs GhosttyKit, builds an opt Boss.app zip, and
# delegates version resolution, tagging, notes, draft creation, asset upload,
# checksum verification, and publish to //tools/release. The GitHub Release
# stays a draft until the zip and its .sha256 sidecar verify.
#
# Asset name Boss-1.0.N.zip and tag scheme boss-v1.0.N are unchanged.
#
# Only triggered on the main branch (see pipeline.yml `if:` condition).
#
# Secret sources (in priority order):
#   1. Env var already set (Pipeline Settings → Environment Variables).
#   2. Buildkite native secrets store via `buildkite-agent secret get`.
#
# See tools/boss/docs/buildkite-release-setup.md and
# tools/boss/docs/buildkite-shake-secrets-setup.md.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

CONFIG="tools/boss/release.toml"

log "[boss-release] releasing"
echo "[boss-release] agent: $(uname -a)"

[[ -x bin/release ]] || die "bin/release is missing; ci-env.sh should have installed it via repobin (REPOBIN.toml tools.release)"

# prepare: skip/idempotency, next boss-v1.0.N, tag HEAD, draft GitHub Release.
# Prints the tag on stdout, or nothing when this run is a no-op (exit 0).
# The tag is pushed before the build so workspace-status.sh can
# `git describe --exact-match` and stamp the binary with 1.0.N.
log "[boss-release] prepare"
TAG="$(bin/release prepare --config "${CONFIG}")"
if [[ -z "${TAG}" ]]; then
  exit 0
fi
[[ "${TAG}" == boss-v* ]] || die "unexpected release tag '${TAG}'; expected boss-v*"

VERSION="${TAG#boss-v}"
ARTIFACT="Boss-${VERSION}.zip"
echo "[boss-release] version: ${TAG}  artifact: ${ARTIFACT}"

# ── read secrets ──────────────────────────────────────────────────────────────

_read_secret() {
  local name="$1"
  # Honour a pre-set env var (Pipeline Settings or local override).
  if [[ -n "${!name:-}" ]]; then
    printf '%s' "${!name}"
    return 0
  fi
  # Buildkite native secrets store.
  if command -v buildkite-agent &>/dev/null; then
    buildkite-agent secret get "$name" 2>/dev/null || true
  fi
}

BOSS_SHAKE_APP_ID=$(_read_secret BOSS_SHAKE_APP_ID)
BOSS_SHAKE_INSTALLATION_ID=$(_read_secret BOSS_SHAKE_INSTALLATION_ID)
BOSS_SHAKE_PRIVATE_KEY_PEM=$(_read_secret BOSS_SHAKE_PRIVATE_KEY_PEM)
export BOSS_SHAKE_APP_ID BOSS_SHAKE_INSTALLATION_ID BOSS_SHAKE_PRIVATE_KEY_PEM

missing=()
[[ -z "${BOSS_SHAKE_APP_ID:-}" ]]           && missing+=("BOSS_SHAKE_APP_ID")
[[ -z "${BOSS_SHAKE_INSTALLATION_ID:-}" ]]  && missing+=("BOSS_SHAKE_INSTALLATION_ID")
[[ -z "${BOSS_SHAKE_PRIVATE_KEY_PEM:-}" ]]  && missing+=("BOSS_SHAKE_PRIVATE_KEY_PEM")

if (( ${#missing[@]} > 0 )); then
  die "Missing Buildkite secrets: ${missing[*]}
Set these in the Buildkite secrets store or in Pipeline Settings → Environment Variables.
See tools/boss/docs/buildkite-shake-secrets-setup.md for step-by-step instructions."
fi

echo "[boss-release] credentials loaded (APP_ID=[REDACTED])"

# ── GhosttyKit stub ───────────────────────────────────────────────────────────
# swift_deps runs `swift package describe` during Bazel analysis, which needs a
# GhosttyKit.xcframework at the gitignored ThirdParty/ path (see the script for
# the full rationale). Materialize a parse-only stub if it's absent.
tools/boss/app-macos/scripts/stub-ghosttykit-xcframework.sh

# ── build Boss.app (optimised, credentials embedded) ─────────────────────────
# Credentials are passed via --define so rules_rust includes them in the rustc
# compile action's cache key + env (option_env! reads them at compile time);
# --action_env alone does not affect the rustc action.
#
# CRITICAL: the build flags below (especially -c opt) change the output
# directory bazel-out is configured into. The path-discovery cquery MUST use
# the IDENTICAL flag set, otherwise it resolves a different configuration's
# output dir — specifically the credential-free `fastbuild` Boss.zip left
# behind by the mac-app-build step (`bazel build //tools/boss/app-macos/...`,
# no -c opt, no creds) — and the smoke test ends up verifying the wrong binary.
# That mismatch is exactly what made every prior fix attempt "pass locally" but
# fail in CI: the credentials were embedded correctly in the opt artifact, but
# the smoke test extracted the fastbuild one. Keep BUILD_FLAGS the single
# source of truth shared by both invocations.
BUILD_FLAGS=(
  -c opt
  --define=BOSS_SHAKE_APP_ID="$BOSS_SHAKE_APP_ID"
  --define=BOSS_SHAKE_INSTALLATION_ID="$BOSS_SHAKE_INSTALLATION_ID"
  --define=BOSS_SHAKE_PRIVATE_KEY_PEM="$BOSS_SHAKE_PRIVATE_KEY_PEM"
)

log "[boss-release] building //tools/boss/app-macos:Boss (opt)"
bazel build "${BUILD_FLAGS[@]}" //tools/boss/app-macos:Boss

# Discover the actual zip output path via cquery, using the SAME BUILD_FLAGS so
# the resolved path matches the configuration we just built (see note above).
log "[boss-release] discovering Boss.zip output path"
ZIP_PATH=$(bazel cquery "${BUILD_FLAGS[@]}" --output=files //tools/boss/app-macos:Boss 2>/dev/null | grep -E '\.zip$' | head -1)

if [[ -z "${ZIP_PATH}" ]]; then
  die "Unable to discover Boss.zip path via cquery. Contents of bazel-bin/tools/boss/app-macos/:
$(ls -la bazel-bin/tools/boss/app-macos/ 2>/dev/null || echo '(directory not found)')"
fi

[[ -f "${ZIP_PATH}" ]] || die "Boss.zip not found at discovered path: ${ZIP_PATH}"
echo "[boss-release] Boss.zip: ${ZIP_PATH}"

log "[boss-release] uploading ${ARTIFACT}"
bin/release upload --config "${CONFIG}" --tag "${TAG}" \
  --asset "${ARTIFACT}=${ZIP_PATH}"

log "[boss-release] publishing ${TAG}"
bin/release publish --config "${CONFIG}" --tag "${TAG}"

log "[boss-release] done — release ${TAG} published"
