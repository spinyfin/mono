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

# Cheap already-tagged skip. ci-env.sh installs bin/release via repobin, which
# builds //tools/release before `prepare` can see that HEAD is already the
# latest published boss-v* commit. On a no-op check that build is most of the
# step. Fetch tags first (Buildkite checkouts do not). Skip here only when:
#   1. resume-draft (RELEASE_RESUME_DRAFT / CHECKLEFT_RESUME_DRAFT) is not 1,
#   2. tools/boss/release.toml parses cleanly,
#   3. HEAD carries a version-family tag, and
#   4. that tag is also the highest published family GitHub Release (drafts
#      excluded; other families such as checkleft ignored). Matches
#      `should_skip` AlreadyReleased: head_sha == last_published_sha.
# A HEAD pinned to an older published family tag, a draft, a missing release,
# a gh error, a config parse error, resume-draft, and an untagged HEAD all
# fall through to `bin/release prepare`. When a family tag is on HEAD (or
# release.toml fails to parse) and the skip is not taken, one stderr line
# names the reason.
# shellcheck source=fetch-boss-tags.sh
source "$(dirname "${BASH_SOURCE[0]}")/fetch-boss-tags.sh"
# shellcheck source=boss-head-release-tag.sh
source "$(dirname "${BASH_SOURCE[0]}")/boss-head-release-tag.sh"
if ! ci_fetch_boss_tags; then
  echo "ERROR: failed to fetch boss-v* tags from origin. Refusing to decide a release from stale tags." >&2
  exit 1
fi
if ! ci_boss_load_release_config "tools/boss/release.toml"; then
  ci_boss_early_skip_not_taken "release.toml parse failed"
elif tag="$(ci_boss_head_release_tag)"; then
  if ci_boss_resume_draft_enabled; then
    ci_boss_early_skip_not_taken "resume-draft set"
  elif ! ci_boss_github_family_release_rows "${CI_BOSS_RELEASE_REPO}"; then
    ci_boss_early_skip_not_taken "gh error: ${CI_BOSS_GH_STDERR}"
  else
    latest="$(printf '%s\n' "$CI_BOSS_RELEASE_ROWS" | ci_boss_select_latest_published_family_tag || true)"
    if [[ -n "$latest" && "$latest" == "$tag" ]]; then
      echo "$(ci_boss_already_released_skip_message "$tag")"
      exit 0
    fi
    head_draft="$(printf '%s\n' "$CI_BOSS_RELEASE_ROWS" | ci_boss_row_draft_for_tag "$tag")"
    if [[ "$head_draft" == "false" ]]; then
      ci_boss_early_skip_not_taken "newer published release exists"
    else
      ci_boss_early_skip_not_taken "release is a draft"
    fi
  fi
fi

# This step may execute its bazel actions on the remote macOS workers; ci-env.sh
# decides whether it does (see remote-execution.sh). For this step, opting in is
# a decision about the shake credentials, which the build below passes through
# --define: the compile action that embeds them, with its environment, and
# every binary built from it are sent to the workers and kept in the remote
# cache. That is accepted because the same values are compiled into the app
# this step publishes, so the cache holds nothing the release does not. A step
# whose secrets must stay on the agent must not copy this line.
CI_REMOTE_EXECUTION_ALLOWED=1
# Tool-install and bin/release stay at opt so they match this step's
# `bazel build -c opt` product builds. --config=release and the shake
# --defines apply only to the Boss.app compile below: they stamp the binary
# and embed credentials, and must not re-key every tool build. That one
# command is the documented exception; it discards the analysis cache when
# those flags appear.
CI_BAZEL_STEP_FLAGS="--compilation_mode=opt"
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

CONFIG="tools/boss/release.toml"

log "[boss-release] releasing"
echo "[boss-release] agent: $(uname -a)"

[[ -x bin/release ]] || die "bin/release is missing; ci-env.sh should have installed it via repobin (REPOBIN.toml tools.release)"

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

# prepare: skip/idempotency, next boss-v1.0.N, tag HEAD, draft GitHub Release.
# Prints the tag on stdout, or nothing when this run is a no-op (exit 0).
# The tag is pushed before the build so workspace-status.sh can
# `git describe --exact-match` and stamp the binary with 1.0.N.
# `--config=release` turns on `--stamp` so build_info_rs and the
# version plist consume that tag; check builds stay on the 0.0.0-dev
# placeholder and keep their action cache across releases.
log "[boss-release] prepare"
TAG="$(bin/release prepare --config "${CONFIG}")"
if [[ -z "${TAG}" ]]; then
  exit 0
fi
echo "[boss-release] release tag: ${TAG}"

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
# CRITICAL: the build flags below (especially -c opt and --config=release)
# change the output directory bazel-out is configured into. The path-discovery
# cquery MUST use the IDENTICAL flag set, otherwise it resolves a different
# configuration's output dir — specifically the credential-free `fastbuild`
# Boss.zip left behind by the mac-app-build step (`bazel build
# //tools/boss/app-macos/...`, no -c opt, no creds, no --stamp) — and the
# smoke test ends up verifying the wrong binary.
# That mismatch is exactly what made every prior fix attempt "pass locally" but
# fail in CI: the credentials were embedded correctly in the opt artifact, but
# the smoke test extracted the fastbuild one. Keep BUILD_FLAGS the single
# source of truth shared by both invocations.
#
# --remote_local_fallback only matters when the step executes remotely, where
# it overrides ci-remote's --noremote_local_fallback: if the workers stop
# answering part-way through, the remaining actions run on this agent instead
# of failing the step. A verification step would rather fail, but by this point
# `prepare` has pushed the tag and created the draft, and a scheduled run does
# not resume a draft, so a failed build here needs a manual run to finish the
# release. It does not cover an action that runs and fails.
#
# This is plain `bazel build`, not bazel_verify_only: the zip is uploaded
# below, so it has to be downloaded to the agent.
BUILD_FLAGS=(
  --config=release
  -c opt
  --define=BOSS_SHAKE_APP_ID="$BOSS_SHAKE_APP_ID"
  --define=BOSS_SHAKE_INSTALLATION_ID="$BOSS_SHAKE_INSTALLATION_ID"
  --define=BOSS_SHAKE_PRIVATE_KEY_PEM="$BOSS_SHAKE_PRIVATE_KEY_PEM"
  --remote_local_fallback
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

# ── verify stamped version in Boss.zip ───────────────────────────────────────
# Release correctness depends on --config=release flipping the stamp_flag select
# so Boss.app's Info.plist and bundled binaries embed the tag-derived version
# instead of the unstamped 0.0.0-dev placeholder. Verify the zip before upload.
log "[boss-release] verifying stamped version in Boss.zip"
EXPECTED_VERSION="${TAG#boss-v}"
VERIFY_DIR=$(mktemp -d)
(
  # Subshell keeps this cleanup trap isolated from ci-env.sh's outer EXIT trap.
  trap 'rm -rf "${VERIFY_DIR}"' EXIT
  unzip -q "${ZIP_PATH}" -d "${VERIFY_DIR}"

  APP_PLIST="${VERIFY_DIR}/Boss.app/Contents/Info.plist"
  [[ -f "${APP_PLIST}" ]] || die "Boss.app/Contents/Info.plist missing from ${ZIP_PATH}"

  for key in CFBundleShortVersionString CFBundleVersion BossFullVersion; do
    actual=$(plutil -extract "${key}" raw -o - "${APP_PLIST}" 2>/dev/null || true)
    if [[ "${actual}" != "${EXPECTED_VERSION}" ]] || [[ "${actual}" == *-dev-* ]]; then
      die "Version mismatch in ${APP_PLIST} for ${key}: got '${actual}', expected '${EXPECTED_VERSION}' (no -dev-)"
    fi
  done

  actual_boss_ver=$("${VERIFY_DIR}/Boss.app/Contents/Resources/bin/boss" --version)
  if [[ "${actual_boss_ver}" != "boss ${EXPECTED_VERSION}" ]]; then
    die "Version mismatch for bundled boss --version: got '${actual_boss_ver}', expected 'boss ${EXPECTED_VERSION}'"
  fi

  actual_bossctl_ver=$("${VERIFY_DIR}/Boss.app/Contents/Resources/bin/bossctl" --version)
  if [[ "${actual_bossctl_ver}" != "bossctl ${EXPECTED_VERSION}" ]]; then
    die "Version mismatch for bundled bossctl --version: got '${actual_bossctl_ver}', expected 'bossctl ${EXPECTED_VERSION}'"
  fi
)
echo "[boss-release] verified version ${EXPECTED_VERSION} in Info.plist, boss, and bossctl"

log "[boss-release] uploading Boss-{version}.zip"
bin/release upload --config "${CONFIG}" --tag "${TAG}" \
  --asset "Boss-{version}.zip=${ZIP_PATH}"

log "[boss-release] publishing ${TAG}"
bin/release publish --config "${CONFIG}" --tag "${TAG}"

log "[boss-release] done — release ${TAG} published"
