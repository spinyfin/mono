#!/usr/bin/env bash
# boss-release.sh — build Boss.app with embedded shake credentials and publish a GitHub Release.
#
# Runs only on main-branch commits (guarded both here and in pipeline.yml via `if:` condition).
# Requires three Buildkite secrets to be set as pipeline environment variables:
#   BOSS_SHAKE_APP_ID, BOSS_SHAKE_INSTALLATION_ID, BOSS_SHAKE_PRIVATE_KEY_PEM
# See tools/boss/docs/buildkite-shake-secrets-setup.md for provisioning instructions.
#
# If any secret is unset this script exits immediately with a human-readable error
# pointing at the setup doc — the bazel build itself would fail anyway, but this
# gives a clearer signal before spending build time.
set -euo pipefail

echo "--- [boss-release] validating secrets"

# Fail fast with a clear pointer to the setup doc rather than a raw bazel error.
: "${BOSS_SHAKE_APP_ID:?BOSS_SHAKE_APP_ID is not set. See tools/boss/docs/buildkite-shake-secrets-setup.md}"
: "${BOSS_SHAKE_INSTALLATION_ID:?BOSS_SHAKE_INSTALLATION_ID is not set. See tools/boss/docs/buildkite-shake-secrets-setup.md}"
: "${BOSS_SHAKE_PRIVATE_KEY_PEM:?BOSS_SHAKE_PRIVATE_KEY_PEM is not set. See tools/boss/docs/buildkite-shake-secrets-setup.md}"

echo "--- [boss-release] building Boss.app with embedded credentials"

# The .bazelrc already has:
#   build --action_env=BOSS_SHAKE_APP_ID
#   build --action_env=BOSS_SHAKE_INSTALLATION_ID
#   build --action_env=BOSS_SHAKE_PRIVATE_KEY_PEM
# so the exported vars below are forwarded into the rustc action automatically.
export BOSS_SHAKE_APP_ID BOSS_SHAKE_INSTALLATION_ID BOSS_SHAKE_PRIVATE_KEY_PEM

bazel build -c opt //tools/boss/app-macos:Boss

# Locate the built .app bundle.
BAZEL_BIN=$(bazel info bazel-bin -c opt)
APP_PATH="${BAZEL_BIN}/tools/boss/app-macos/Boss.app"
[[ -d "$APP_PATH" ]] || {
  echo "ERROR: Boss.app not found at expected path: ${APP_PATH}" >&2
  exit 1
}

echo "--- [boss-release] computing next version tag"

# Query all release tags, filter to v1.0.N pattern, take max N, increment by 1.
# If no matching releases exist, start at v1.0.0.
EXISTING_TAGS=$(gh release list --repo spinyfin/mono --limit 200 --json tagName -q '.[].tagName' 2>/dev/null || echo "")
MAX_N=-1
while IFS= read -r tag; do
  [[ -z "$tag" ]] && continue
  if [[ "$tag" =~ ^v?1\.0\.([0-9]+)$ ]]; then
    n="${BASH_REMATCH[1]}"
    (( n > MAX_N )) && MAX_N=$n
  fi
done <<< "$EXISTING_TAGS"

NEXT_N=$(( MAX_N + 1 ))
VERSION="v1.0.${NEXT_N}"
ARTIFACT="Boss-1.0.${NEXT_N}.zip"

echo "[boss-release] next version: ${VERSION}"

echo "--- [boss-release] packaging Boss.app"

STAGING=$(mktemp -d -t boss-release)
trap 'rm -rf "$STAGING"' EXIT

ZIP_PATH="${STAGING}/${ARTIFACT}"

# Use ditto for correct macOS .app packaging — preserves resource forks,
# Finder metadata, and extended attributes that naive zip strips.
ditto -c -k --sequesterRsrc --keepParent "$APP_PATH" "$ZIP_PATH"

echo "--- [boss-release] creating GitHub Release ${VERSION}"

# --generate-notes fills the release body with a "What's changed since last
# release" section from GitHub's automatic changelog feature.  No manual
# curation needed for now.
gh release create "${VERSION}" \
  --repo spinyfin/mono \
  --title "Boss 1.0.${NEXT_N}" \
  --generate-notes \
  "${ZIP_PATH}#${ARTIFACT}"

echo "[boss-release] published ${VERSION} — artifact: ${ARTIFACT}"
