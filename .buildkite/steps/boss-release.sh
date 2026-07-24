#!/usr/bin/env bash
# boss-release.sh — post-merge release step.
#
# Builds Boss.app with the three shake credentials embedded, zips it, and
# creates a GitHub Release on spinyfin/mono tagged boss-v1.0.N where N is one
# greater than the highest existing boss-v1.0.* release.
#
# Only triggered on the main branch (see pipeline.yml `if:` condition).
# Skips (exit 0) when the merge does not touch anything under tools/boss/.
# Retries the asset upload step on transient failures; the release record is created first.
#
# Secret sources (in priority order):
#   1. Env var already set (Pipeline Settings → Environment Variables).
#   2. Buildkite native secrets store via `buildkite-agent secret get`.
#
# See tools/boss/docs/buildkite-shake-secrets-setup.md for provisioning.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

log "[boss-release] releasing"
echo "[boss-release] agent: $(uname -a)"

# ── list all boss-v* releases (single authoritative snapshot) ────────────────
# A single release-list call, retried on transient failure, backs every
# decision below (LAST_TAG, the idempotency guard, change-detection, and the
# next-version computation). Two independent queries used to run minutes
# apart in this script; a `|| true` on the first swallowed any `gh` failure
# (auth hiccup, API 5xx, rate limit) into an EMPTY LAST_TAG, indistinguishable
# from "no prior release exists" — which silently triggered the misleading
# "Initial Boss release." placeholder even though prior releases existed
# (observed on boss-v1.0.263, cut 23 releases after boss-v1.0.0). Query once,
# fail loudly if `gh` can't be trusted, and reuse the result everywhere so the
# view of "what releases exist" cannot go stale or disagree with itself
# mid-script.
#
# Fetched via the REST endpoint (`gh api repos/.../releases`), NOT
# `gh release list` — that command queries GraphQL under the hood, which
# shares a rate-limit budget with unrelated pollers on this token (see T3192).
# GraphQL exhaustion was confirmed as the trigger for build 8383's collision
# (boss-v1.0.328 recomputed and re-tagged ~4 min after it was already
# published): a degraded/truncated GraphQL response under-reported the true
# max tag. REST releases live on the separate `core` rate-limit budget, so
# exhausting `graphql` here can no longer corrupt version numbering.
BUILDKITE_SOURCE="${BUILDKITE_SOURCE:-}"

_gh_release_list_json() {
  local out attempt
  for attempt in 1 2 3; do
    if out=$(gh api repos/spinyfin/mono/releases --paginate -X GET -F per_page=100 2>&1); then
      printf '%s' "${out}"
      return 0
    fi
    echo "[boss-release] release list attempt ${attempt}/3 failed: ${out}" >&2
    sleep $((attempt * 5))
  done
  return 1
}

log "[boss-release] listing existing boss-v* releases"
RELEASE_LIST_JSON=$(_gh_release_list_json) || die \
  "Unable to list existing releases via the GitHub releases API after 3 attempts. Refusing to proceed: treating this as \"no prior release\" would risk publishing a misleading \"Initial Boss release.\" placeholder, or recomputing the next version number, when releases may already exist. Investigate GitHub API health and retry."

log "[boss-release] resolving last boss-v* release tag"
LAST_TAG=$(echo "${RELEASE_LIST_JSON}" \
  | jq -r '[.[] | select(.tag_name | test("^boss-v1\\.0\\.[0-9]+$"))] | .[0].tag_name // empty')

LAST_SHA=""
if [[ -n "${LAST_TAG}" ]]; then
  # BK checkouts are shallow (single-commit fetch, no --tags). Fetch the
  # specific release tag so git rev-list can resolve it locally.
  git fetch origin "refs/tags/${LAST_TAG}:refs/tags/${LAST_TAG}" 2>/dev/null || true

  LAST_SHA=$(git rev-list -n 1 "${LAST_TAG}" 2>/dev/null || true)

  if [[ -z "${LAST_SHA}" ]]; then
    # Local resolution still failed (annotated tag, fetch blocked, etc.).
    # Fall back to GitHub API — resolves both lightweight and annotated tags.
    echo "[boss-release] ${LAST_TAG} not in local refs; querying GitHub API"
    LAST_SHA=$(gh api "repos/spinyfin/mono/commits/${LAST_TAG}" \
      --jq '.sha' 2>/dev/null || true)
  fi
fi

# ── idempotency guard: never re-release an already-tagged commit (ALL paths) ─
# This check is SEPARATE from change-detection and applies to every trigger,
# including manual / API runs. Rationale: a manual re-trigger on an unchanged
# main branch would otherwise bump MAX_N and cut a new version on the same
# commit (observed as boss-v1.0.10 and boss-v1.0.11 both on commit 484ea18).
#
# If a deliberate re-cut is ever required, delete the tag first or use a
# dedicated --force flag; do NOT rely on manual triggering.
if [[ -n "${LAST_SHA}" ]]; then
  HEAD_SHA=$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")
  if [[ "${HEAD_SHA}" == "${LAST_SHA}" ]]; then
    echo "release step skipped: HEAD (${HEAD_SHA:0:12}) is already the commit for ${LAST_TAG} — re-releasing the same commit is a no-op"
    exit 0
  fi
fi

# ── guard: skip if no Boss-affecting changes (cron path only) ─────────────────
# For scheduled (cron) builds, only publish a release when there are
# Boss-affecting changes since the last boss-v* tag. A cron run with no Boss
# changes exits 0 silently.
#
# For manual triggers (BUILDKITE_SOURCE == "ui" or "api"), skip change
# detection entirely — the operator explicitly asked for a release.
#
# Paths that count as Boss-affecting:
#   - tools/boss/** — the binary's source code
#   - .buildkite/steps/boss-release.sh — the release script itself
#   - .buildkite/pipeline.yml — the release wiring

if [[ "${BUILDKITE_SOURCE}" == "ui" || "${BUILDKITE_SOURCE}" == "api" ]]; then
  echo "[boss-release] manual trigger via ${BUILDKITE_SOURCE}; skipping change-detection"
else
  log "[boss-release] checking for Boss-affecting changes since last tag"

  if [[ -z "${LAST_TAG}" ]]; then
    echo "[boss-release] no previous boss-v* tag found; proceeding with first release"
  elif [[ -z "${LAST_SHA}" ]]; then
    echo "[boss-release] WARNING: could not resolve tag ${LAST_TAG} by any means; proceeding"
  else
    # Unshallow if needed so git diff can reach LAST_SHA.
    if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
      echo "[boss-release] unshallowing repo for full diff"
      git fetch --unshallow origin 2>/dev/null || true
    fi

    TOUCHED=$(git diff --name-only "${LAST_SHA}..HEAD" 2>/dev/null || true)
    BOSS_TOUCHED=$(echo "${TOUCHED}" | grep -E "^(tools/boss/|\.buildkite/steps/boss-release\.sh|\.buildkite/pipeline\.yml)" || true)

    if [[ -z "${BOSS_TOUCHED}" ]]; then
      TOUCHED_SUMMARY=$(echo "${TOUCHED}" | tr '\n' ' ')
      echo "release step skipped: no Boss-affecting changes since ${LAST_TAG} (touched: ${TOUCHED_SUMMARY})"
      exit 0
    fi
    echo "[boss-release] Boss-affecting changes detected since ${LAST_TAG}; proceeding"
  fi
fi

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

# ── fetch tags so workspace-status.sh gets a git-derived version ──────────────
# Buildkite clones are shallow by default and do not carry all remote tags.
# tools/boss/installer/workspace-status.sh relies on `git describe --tags
# --match "boss-v*"` to derive STABLE_BOSS_VERSION.  Without the tags that
# command returns empty and the version falls back to "0.0.0-dev-<sha>".
# Fetching all tags here guarantees the describe call works and means the
# binary embeds the real release version string (see version-tag section below).
log "[boss-release] fetching boss-v* tags for version stamping"
git fetch --tags origin 2>/dev/null || true

# ── compute next release version ─────────────────────────────────────────────
# Tags match boss-v1.0.N (monorepo-prefixed, mirrors checkleft-v* convention).
# If no matching release exists yet, start at boss-v1.0.0.
#
# IMPORTANT: this block is intentionally placed BEFORE the bazel build so that
# the next-version tag can be pushed to the remote before Bazel runs
# workspace-status.sh.  That lets `git describe --tags --match "boss-v*"
# --exact-match` hit the tag and stamp the binary with the exact release
# version (e.g. "1.0.5") rather than a dev suffix.

log "[boss-release] computing next version"
# Reuse the single RELEASE_LIST_JSON snapshot from above instead of issuing a
# second release-list call — two separate calls run seconds to minutes
# apart could observe different states (a release created in between) and
# silently disagree about the last tag vs. the max existing tag.
EXISTING_TAGS=$(echo "${RELEASE_LIST_JSON}" | jq -r '.[].tag_name')

MAX_N=-1
while IFS= read -r tag; do
  if [[ "${tag}" =~ ^boss-v1\.0\.([0-9]+)$ ]]; then
    n="${BASH_REMATCH[1]}"
    if (( n > MAX_N )); then MAX_N="${n}"; fi
  fi
done <<< "${EXISTING_TAGS}"

# ── degraded-list cross-check ─────────────────────────────────────────────────
# The release-list snapshot above is API data (REST now, previously GraphQL)
# and can under-report the true max on a degraded/rate-limited response
# without failing outright (build 8383: a truncated snapshot silently missed
# boss-v1.0.328, so this block recomputed 328 again and collided with its own
# prior tag). Cross-check against `git ls-remote --tags`, which talks the git
# smart-HTTP protocol directly to the remote and shares no budget with either
# GitHub API. If git sees a higher boss-v1.0.* tag than the release list
# reported, the list is untrustworthy — fail loudly (matching the existing
# fail-closed behavior for an outright `gh` failure above) instead of silently
# computing an already-published version number.
GIT_MAX_N=-1
while IFS= read -r tag; do
  if [[ "${tag}" =~ ^boss-v1\.0\.([0-9]+)$ ]]; then
    n="${BASH_REMATCH[1]}"
    if (( n > GIT_MAX_N )); then GIT_MAX_N="${n}"; fi
  fi
done < <(git ls-remote --tags origin 'refs/tags/boss-v1.0.*' 2>/dev/null \
  | sed -E 's#.*refs/tags/##')

if (( GIT_MAX_N > MAX_N )); then
  die "Release-list snapshot reports the highest boss-v1.0.* release as N=${MAX_N}, but 'git ls-remote --tags origin' (independent of the GitHub releases API) shows a tag up to boss-v1.0.${GIT_MAX_N}. The release list is degraded or incomplete — refusing to compute a version number from it, since doing so would recompute and collide with an already-published tag. Investigate GitHub API health (rate limits, outages) and retry."
fi

NEXT_N=$(( MAX_N + 1 ))
VERSION="boss-v1.0.${NEXT_N}"
ARTIFACT="Boss-1.0.${NEXT_N}.zip"
echo "[boss-release] version: ${VERSION}  artifact: ${ARTIFACT}"

# Push the release tag to the remote BEFORE building so that
# workspace-status.sh can resolve it via `git describe --exact-match` and
# stamp the binary with the clean "1.0.N" version string.
TAG_PUSHED=0
NOTES_FILE=""
WORK_DIR=""
RELEASE_CREATED=0

# Single EXIT trap — handles every failure path after the tag is pushed.
# Mirrors checkleft-release.sh's TAG_PUSHED-guarded cleanup pattern.
_cleanup() {
  [[ -n "${NOTES_FILE}" ]] && rm -f "${NOTES_FILE}"
  if [[ "${TAG_PUSHED}" == "1" && "${RELEASE_CREATED}" == "0" ]]; then
    echo "[boss-release] release not completed — deleting leaked remote tag ${VERSION}" >&2
    git push origin ":refs/tags/${VERSION}" 2>/dev/null \
      || echo "[boss-release] WARNING: failed to delete leaked tag ${VERSION} — manual cleanup needed: git push origin :refs/tags/${VERSION}" >&2
    git tag -d "${VERSION}" 2>/dev/null || true
  fi
  [[ -n "${WORK_DIR}" ]] && rm -rf "${WORK_DIR}"
}
trap '_cleanup' EXIT

# ── tag-collision guard ────────────────────────────────────────────────────────
# Re-fetch immediately before tagging to close the race window between the
# release-list snapshot above and now (mirrors checkleft-release.sh). If
# ${VERSION} already exists on the remote — the exact build-8383 failure mode,
# where a degraded list snapshot recomputed an already-published number — this
# is EITHER a redundant duplicate run on a commit that's already released
# (idempotent no-op: exit 0, nothing left to do) OR a genuinely stale/wrong
# computation (die loudly with an actionable message instead of the raw
# `fatal: tag already exists` that used to hard-abort here).
git fetch origin "refs/tags/${VERSION}:refs/tags/${VERSION}" 2>/dev/null || true
if git rev-parse -q --verify "refs/tags/${VERSION}" &>/dev/null; then
  EXISTING_TAG_SHA=$(git rev-list -n 1 "${VERSION}" 2>/dev/null || true)
  HEAD_SHA_FOR_TAG=$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")
  if [[ -n "${EXISTING_TAG_SHA}" && "${EXISTING_TAG_SHA}" == "${HEAD_SHA_FOR_TAG}" ]]; then
    echo "release step skipped: ${VERSION} already exists and points at HEAD (${HEAD_SHA_FOR_TAG:0:12}) — this run is a redundant duplicate of an already-published release; nothing to do"
    exit 0
  fi
  die "Computed tag ${VERSION} already exists on remote (at commit ${EXISTING_TAG_SHA:-unknown}), but HEAD is ${HEAD_SHA_FOR_TAG:-unknown}. The version resolver produced a stale or already-published result — most likely a degraded release-list snapshot under-reported the true max. Re-run the pipeline to retry with a fresh snapshot; do NOT force-push over the existing tag."
fi

log "[boss-release] creating and pushing release tag ${VERSION} (before build)"
git tag "${VERSION}" HEAD
git push origin "refs/tags/${VERSION}"
TAG_PUSHED=1

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

# ── prepare the pre-zipped artifact ────────────────────────────────────────────
# The macos_application rule pre-zips the bundle, so we just rename it to the
# release version and prepare it for publication.

log "[boss-release] preparing ${ARTIFACT}"
WORK_DIR=$(mktemp -d -t boss-release)

cp "${ZIP_PATH}" "${WORK_DIR}/${ARTIFACT}"
echo "[boss-release] artifact: $(du -sh "${WORK_DIR}/${ARTIFACT}" | cut -f1)"

# ── create GitHub Release ─────────────────────────────────────────────────────
# Split into three independent steps to isolate failure modes and enable
# selective retry on the (flaky) asset-upload step.

log "[boss-release] generating release notes for ${VERSION}"

# Sanity check before trusting the "no prior release" path: if other boss-v*
# tags exist (MAX_N >= 0) but LAST_TAG came up empty, the two views of
# release history disagree — do NOT paper over that with the "Initial Boss
# release." placeholder. Fail loudly so the inconsistency gets investigated
# instead of silently publishing misleading notes.
if [[ -z "${LAST_TAG}" && "${MAX_N}" -ge 0 ]]; then
  die "LAST_TAG resolved empty but existing boss-v* releases were found (highest: boss-v1.0.${MAX_N}). Refusing to publish '${VERSION}' with an 'Initial Boss release.' placeholder — this indicates a bug in tag resolution, not a genuine first release."
fi

NOTES_FILE="$(mktemp /tmp/boss-release-notes-XXXXXX.md)"
if [[ -n "${LAST_TAG}" ]]; then
  # Ensure full history for git log — a shallow clone silently truncates the
  # commit range returned by changelog, including on manual (ui/api) triggers
  # where the change-detection unshallow is skipped.
  if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
    echo "[boss-release] unshallowing repo for changelog"
    git fetch --unshallow origin 2>/dev/null || true
  fi
  bin/changelog \
    --project tools/boss/PROJECT.yaml \
    --from "${LAST_TAG}" \
    --to "${VERSION}" \
    --repo spinyfin/mono \
    --enrich \
    > "${NOTES_FILE}"
else
  printf 'Initial Boss release.\n' > "${NOTES_FILE}"
fi

log "[boss-release] creating GitHub Release ${VERSION}"
gh release create "${VERSION}" \
  --repo spinyfin/mono \
  --title "Boss ${VERSION#boss-v}" \
  --notes-file "${NOTES_FILE}"
RELEASE_CREATED=1

log "[boss-release] uploading asset with retry"
UPLOAD_OK=0
for attempt in 1 2 3; do
  if gh release upload "${VERSION}" "${WORK_DIR}/${ARTIFACT}" \
      --repo spinyfin/mono --clobber; then
    UPLOAD_OK=1
    break
  fi
  echo "[boss-release] upload attempt ${attempt} failed; sleeping $((attempt * 15))s before retry"
  sleep $((attempt * 15))
done

if (( UPLOAD_OK != 1 )); then
  die "release ${VERSION} created but asset upload failed after 3 attempts; manually upload via 'gh release upload ${VERSION} <path>' or delete the empty release with 'gh release delete ${VERSION}'"
fi

log "[boss-release] done — release ${VERSION} published"
