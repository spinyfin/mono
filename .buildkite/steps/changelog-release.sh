#!/usr/bin/env bash
# changelog-release.sh — cross-platform release step for the `changelog` binary.
#
# Modeled on .buildkite/steps/checkleft-release.sh. changelog ships as a
# prebuilt binary consumed by external repos (repobin's registered
# //tools/changelog:changelog target builds from source, which is unworkable
# for a consumer that wants a pinned, checksum-verified binary with no cold
# Bazel/mono checkout), so this step bumps the version, tags the release,
# builds the binaries for every supported platform, and publishes them as
# assets on a GitHub Release.
#
# Deliberately SIMPLER than checkleft's release script in two ways:
#   - No musl phase. changelog has no C dependencies (unlike checkleft, which
#     carries wasmtime and tree-sitter and needs a hermetic musl toolchain),
#     so there is nothing that benefits from a static musl build; native
#     builds for the two required platforms are enough.
#   - No version-embedding step. changelog's CLI has no `--version` flag and
#     never reads CARGO_PKG_VERSION, so there is nothing to patch into the
#     checkout before building — the tag is the only place the version lives,
#     and a given commit produces byte-identical binaries regardless of which
#     tag names it.
#
# Phases:
#   prepare — the orchestrator. Runs the skip-logic, computes the next patch
#             version, tags the release commit, and creates the GitHub
#             Release as a DRAFT. Hands the tag to the build phases via
#             buildkite-agent meta-data. If the newest changelog-v* release is
#             already a draft for this exact commit (an interrupted prior
#             run), resumes that tag/draft instead of computing a new
#             version — see the resume-existing-draft block in phase_prepare.
#   linux   — builds the Linux binary and uploads it to the release.
#   darwin  — builds the macOS binaries and uploads them to the release.
#   publish — verifies every REQUIRED_ASSETS entry (plus any present
#             OPTIONAL_ASSETS entry) is on the release and checksum-correct,
#             then flips the release from draft to published. This is the
#             ONLY step that publishes — the release is never public with a
#             missing or unverified asset. A verification failure leaves the
#             draft (and its assets) in place for inspection and fails the
#             build.
#
# The linux and darwin build phases both depend only on `prepare`, so they
# run in PARALLEL on separate agents; `publish` depends on both, so
# wall-clock is prepare + max(linux, darwin) + publish.
#
# The version bump is NEVER committed to main. It is recorded only in the git
# tag + GitHub Release. Cargo.toml's `version` field is read as a starting
# point for the next tag's patch counter but is never rewritten in the
# checkout — since nothing embeds it in the binary, there is nothing to
# patch before building.
#
# Trigger model (see tools/changelog/docs/buildkite-release-setup.md):
#   - scheduled (cron) builds  → skip if nothing under changelog changed since
#                                the last changelog-v* tag.
#   - manual builds (ui / api) → always release.
#
# Auth: releases run on the CI agents' ambient git + `gh` credentials (every
# worker can push to the repo), exactly like checkleft-release.sh — the tag
# is pushed with `git push origin` and the GitHub Release is created with
# `gh`. No dedicated release token is needed.
set -euo pipefail

# ── configuration ─────────────────────────────────────────────────────────────
REPO="spinyfin/mono"
TAG_PREFIX="changelog-v"
CARGO_TOML="tools/changelog/Cargo.toml"
BIN_TARGET="//tools/changelog:changelog"
ASSET_PREFIX="changelog"
META_TAG_KEY="changelog-release-tag"

# Expected asset set — declared explicitly so the publish phase verifies
# against a known-complete list rather than "whatever happened to upload".
# Each entry gets an accompanying "<name>.sha256" sidecar (see stage_asset).
#
# REQUIRED_ASSETS must ALL be present and checksum-verified before the
# release is published; a missing or mismatched required asset fails the
# release and leaves it as a draft. OPTIONAL_ASSETS are checksum-verified if
# present but do not block publish if absent — this mirrors checkleft's
# darwin x86_64 fallback (build_cross_cargo failure there only warns, see
# phase_darwin).
#
# Platform set: aarch64-apple-darwin is required because it is the concrete
# consumer that motivated this pipeline (flunge's macOS release agents, per
# appoint's release step). x86_64-unknown-linux-gnu is required too because
# mono's own CI (and any Linux Buildkite consumer, e.g. a future appoint
# Linux release agent) needs a Linux binary and a native Bazel build for it
# is essentially free alongside the macOS build. x86_64-apple-darwin is
# optional, matching checkleft, since it is a best-effort cargo cross-build
# from the arm64 macOS agent rather than a native target.
REQUIRED_ASSETS=(
  "${ASSET_PREFIX}-aarch64-apple-darwin"
  "${ASSET_PREFIX}-x86_64-unknown-linux-gnu"
)
OPTIONAL_ASSETS=(
  "${ASSET_PREFIX}-x86_64-apple-darwin"
)

# Mutable state read by the EXIT trap. Globals (not function-locals) so the trap
# can reference them under `set -u` after the phase function has returned.
STAGE=""
TAG_PUSHED=0
LOCAL_TAG_CREATED=0
NEW_TAG=""
NOTES_FILE=""

# changelog-affecting paths for change-detection. Mirrors checkleft-release.sh's
# scoping: the binary's source, the release script, and the pipeline wiring.
CHANGE_PATHS_RE='^(tools/changelog/|\.buildkite/steps/changelog-release\.sh|\.buildkite/pipeline-changelog-release\.yml|\.buildkite/pipeline-changelog-release-builds\.yml)'

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

die() { echo "ERROR: $*" >&2; exit 1; }
log() { echo "--- $*"; }

# ── buildkite meta-data helpers (env-overridable for local dry runs) ──────────

meta_set() {
  command -v buildkite-agent &>/dev/null || return 0
  buildkite-agent meta-data set "$1" "$2"
}

meta_get() {
  local key="$1"
  if command -v buildkite-agent &>/dev/null; then
    buildkite-agent meta-data get "$key" --default "" 2>/dev/null || true
  fi
}

# ── version helpers ───────────────────────────────────────────────────────────

# sha256 of a file, on either platform.
_sha256() {
  if command -v sha256sum &>/dev/null; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

# compute_next_version — sets BASE, NEW_VERSION, NEW_TAG from the current
# Cargo.toml version, cross-checked against existing changelog-v* releases so
# a stale Cargo.toml can never reuse an already-published version. Unlike
# checkleft's alpha-counter scheme, changelog's Cargo.toml carries a plain
# X.Y.Z (no pre-release suffix), so this pipeline revs the PATCH counter
# within the Cargo.toml major.minor — bump major/minor by hand if intended.
compute_next_version() {
  local cur
  cur="$(grep -E '^version = "' "${CARGO_TOML}" | head -1 | sed -E 's/^version = "(.*)"/\1/')"
  [[ -n "${cur}" ]] || die "could not read version from ${CARGO_TOML}"

  if [[ ! "${cur}" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
    die "changelog version '${cur}' is not in the expected X.Y.Z form (no pre-release suffix); this pipeline only revs the patch counter — bump major/minor by hand if intended"
  fi
  local major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" cargo_patch="${BASH_REMATCH[3]}"
  BASE="${major}.${minor}"

  # Highest existing patch among git tags for this major.minor.
  # Requires the caller to have run `git fetch --tags --prune --prune-tags` first
  # so local tags mirror the current remote state. This avoids silently falling
  # back to the Cargo.toml patch counter when a `gh` API call fails.
  local release_max=-1 tag n
  while IFS= read -r tag; do
    if [[ "${tag}" =~ ^${TAG_PREFIX}${BASE}\.([0-9]+)$ ]]; then
      n="${BASH_REMATCH[1]}"
      if (( n > release_max )); then release_max="${n}"; fi
    fi
  done < <(git tag -l "${TAG_PREFIX}${BASE}.*")

  local highest="${cargo_patch}"
  if (( release_max > highest )); then highest="${release_max}"; fi
  local next=$(( highest + 1 ))

  CUR_VERSION="${cur}"
  NEW_VERSION="${BASE}.${next}"
  NEW_TAG="${TAG_PREFIX}${NEW_VERSION}"
}

# ── build helpers ─────────────────────────────────────────────────────────────

# build_native_bazel — optimized native binary via bazel, echoes its path.
# changelog embeds no version string, so unlike checkleft's equivalent helper
# this needs no prior Cargo.toml edit for the build to be "correct" — every
# build of the same commit produces the same bytes regardless of NEW_VERSION.
build_native_bazel() {
  log "[changelog-release] bazel build -c opt ${BIN_TARGET}" >&2
  bazel build -c opt "${BIN_TARGET}" >&2
  local path
  # ci-env.sh's bazel() wrapper does `2>&1 | tee`, merging bazel's progress and
  # INFO messages (normally on stderr) into stdout. `2>/dev/null` here therefore
  # only suppresses the wrapper function's own stderr (empty) — it does NOT
  # suppress bazel's merged informational output. With a cold or invalidated
  # analysis cache, those INFO/Loading/Analyzing lines arrive before the actual
  # file path in the merged stream, causing `head -1` to capture a message line
  # instead of the path. Filter to `bazel-out/…` lines, which is the invariant
  # prefix for all `--output=files` paths regardless of --output_user_root.
  # `|| true`: grep/head can SIGPIPE cquery under pipefail; the guard below reports.
  path="$(bazel cquery -c opt --output=files "${BIN_TARGET}" 2>/dev/null \
    | grep '^bazel-out/' | head -1 || true)"
  [[ -n "${path}" && -f "${path}" ]] || { echo "could not locate bazel binary output" >&2; return 1; }
  echo "${path}"
}

# build_cross_cargo TRIPLE — cross binary via cargo, echoes its path.
# Returns non-zero (without aborting the script) if the target toolchain is
# unavailable, so the optional darwin x86_64 asset can be skipped gracefully.
build_cross_cargo() {
  local triple="$1"
  log "[changelog-release] rustup target add ${triple}" >&2
  rustup target add "${triple}" >&2 || { echo "rustup target add ${triple} failed" >&2; return 1; }
  log "[changelog-release] cargo build --release -p git_changelog --target ${triple}" >&2
  cargo build --release --locked -p git_changelog --target "${triple}" >&2 || {
    echo "cargo build for ${triple} failed" >&2; return 1; }
  local path="target/${triple}/release/changelog"
  [[ -f "${path}" ]] || { echo "expected ${path} not produced" >&2; return 1; }
  echo "${path}"
}

# stage_asset SRC NAME — copy SRC to the staging dir as NAME and write NAME.sha256.
stage_asset() {
  local src="$1" name="$2" sum
  cp -L "${src}" "${STAGE}/${name}"
  chmod +x "${STAGE}/${name}"
  sum="$(_sha256 "${STAGE}/${name}")"
  # Sidecar references the bare asset name so `sha256sum -c` works from the dir.
  echo "${sum}  ${name}" > "${STAGE}/${name}.sha256"
  echo "[changelog-release] staged ${name} ($(du -h "${STAGE}/${name}" | cut -f1))"
}

# upload_release_assets — upload every staged file to the release, with retry.
upload_release_assets() {
  local ok=0 attempt
  for attempt in 1 2 3; do
    if gh release upload "${NEW_TAG}" "${STAGE}"/* --repo "${REPO}" --clobber; then
      ok=1; break
    fi
    echo "[changelog-release] upload attempt ${attempt} failed; sleeping $((attempt * 15))s"
    sleep $((attempt * 15))
  done
  (( ok == 1 )) || die "asset upload failed after 3 attempts for ${NEW_TAG}; retry the job, or upload manually with 'gh release upload ${NEW_TAG} <files> --repo ${REPO} --clobber'"
}

# ── skip-logic (linux phase only) ─────────────────────────────────────────────

is_manual() {
  [[ "${BUILDKITE_SOURCE:-}" == "ui" || "${BUILDKITE_SOURCE:-}" == "api" ]]
}

# _resolve_tag_sha TAG — echoes the commit sha a tag points at (empty if TAG is
# empty or unresolvable). Fetches the tag ref first so a warm agent workspace
# sees it even if it predates the workspace's last full fetch.
_resolve_tag_sha() {
  local tag="$1" sha=""
  if [[ -n "${tag}" ]]; then
    git fetch origin "refs/tags/${tag}:refs/tags/${tag}" 2>/dev/null || true
    sha="$(git rev-list -n 1 "${tag}" 2>/dev/null || true)"
    if [[ -z "${sha}" ]]; then
      sha="$(gh api "repos/${REPO}/commits/${tag}" --jq '.sha' 2>/dev/null || true)"
    fi
  fi
  echo "${sha}"
}

# resolve_last_release — sets LAST_TAG/LAST_SHA (newest PUBLISHED changelog-v*
# release only) and LAST_DRAFT_TAG/LAST_DRAFT_SHA (newest DRAFT changelog-v*
# release only), kept strictly separate because their consumers want different
# things: should_skip()'s idempotency guard and the changelog `--from` range
# must never land on a draft — a release that was never published is not "the
# last release" for either change-detection or changelog purposes, and doing
# so would silently drop the commits/changes between a stranded draft and the
# next real release. Only phase_prepare's resume check wants the draft.
resolve_last_release() {
  log "[changelog-release] resolving last ${TAG_PREFIX}* release"
  local releases_json
  releases_json="$(gh release list --repo "${REPO}" --limit 300 --json tagName,isDraft \
    --jq "[.[] | select(.tagName | startswith(\"${TAG_PREFIX}\"))]" 2>/dev/null || true)"
  releases_json="${releases_json:-[]}"
  [[ "${releases_json}" == "null" ]] && releases_json="[]"

  LAST_TAG="$(jq -r '[.[] | select(.isDraft == false)][0].tagName // empty' <<<"${releases_json}")"
  LAST_DRAFT_TAG="$(jq -r '[.[] | select(.isDraft == true)][0].tagName // empty' <<<"${releases_json}")"

  LAST_SHA="$(_resolve_tag_sha "${LAST_TAG}")"
  LAST_DRAFT_SHA="$(_resolve_tag_sha "${LAST_DRAFT_TAG}")"
}

# should_skip — echoes a skip reason and returns 0 when this run is a no-op.
should_skip() {
  local head_sha
  head_sha="$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")"

  # Idempotency guard (all trigger paths, including manual): never re-release a
  # commit that is already the head of the latest release tag.
  if [[ -n "${LAST_SHA}" && "${head_sha}" == "${LAST_SHA}" ]]; then
    echo "release skipped: HEAD (${head_sha:0:12}) is already ${LAST_TAG} — re-releasing the same commit is a no-op"
    return 0
  fi

  if is_manual; then
    echo ""  # manual trigger always proceeds
    return 1
  fi

  if [[ -z "${LAST_TAG}" ]]; then
    echo ""  # no prior release; proceed with the first
    return 1
  fi
  if [[ -z "${LAST_SHA}" ]]; then
    echo ""  # could not resolve tag SHA; proceed rather than silently stall
    return 1
  fi

  if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
    git fetch --unshallow origin 2>/dev/null || true
  fi

  local touched changelog_touched
  touched="$(git diff --name-only "${LAST_SHA}..HEAD" 2>/dev/null || true)"
  changelog_touched="$(echo "${touched}" | grep -E "${CHANGE_PATHS_RE}" || true)"
  if [[ -z "${changelog_touched}" ]]; then
    echo "release skipped: no changelog-affecting changes since ${LAST_TAG}"
    return 0
  fi
  echo ""
  return 1
}

# cleanup — single EXIT trap. Removes the staging dir and, if a tag was pushed
# but the release never completed, deletes the leaked remote tag. No commit is
# ever pushed to main, so there is nothing else to unwind. All state is read
# defensively for `set -u` safety.
#
# `return 0` is REQUIRED: as the EXIT trap, this function's exit status becomes
# the script's. Phases that never set STAGE (prepare; a skipped run; a build
# phase that finds no tag) would otherwise end on the false `[[ -n "" ]]` test
# and exit 1 despite succeeding.
cleanup() {
  if [[ "${TAG_PUSHED}" == "1" && -n "${NEW_TAG}" ]]; then
    echo "[changelog-release] release did not complete — deleting leaked remote tag ${NEW_TAG}" >&2
    git push origin ":refs/tags/${NEW_TAG}" 2>/dev/null || true
    git tag -d "${NEW_TAG}" 2>/dev/null || true
  elif [[ "${LOCAL_TAG_CREATED}" == "1" && -n "${NEW_TAG}" ]]; then
    # git tag ran but the push failed; clean up the local tag so a re-run of
    # the pipeline on a warm workspace doesn't hit "tag already exists".
    echo "[changelog-release] cleaning up local tag ${NEW_TAG} after push failure" >&2
    git tag -d "${NEW_TAG}" 2>/dev/null || true
  fi
  [[ -n "${NOTES_FILE}" ]] && rm -f "${NOTES_FILE}"
  [[ -n "${STAGE}" ]] && rm -rf "${STAGE}"
  return 0
}

# ── phases ────────────────────────────────────────────────────────────────────

phase_prepare() {
  echo "[changelog-release] agent: $(uname -a)"
  # Sync remote tags before any version resolution. Warm agent workspaces can
  # have a stale tag set, causing the resolver to compute a version that was
  # already published. --prune-tags also removes locally-cached tags that have
  # been deleted from the remote since the last checkout.
  git fetch --tags --prune --prune-tags origin \
    || die "git fetch --tags failed; aborting release to avoid computing a stale next version"
  resolve_last_release

  # Resume-existing-draft check: if the newest changelog-v* release is for THIS
  # commit and is still a draft, a prior run got interrupted before publishing
  # (e.g. the pipeline was re-triggered on the same commit after a build or
  # publish phase failed). should_skip()'s idempotency guard would otherwise
  # treat "HEAD == last release commit" as "nothing to do" — which is correct
  # for an already PUBLISHED release, but wrong here: a draft means the
  # release never finished, so silently skipping would strand it forever.
  # Resume it deterministically by re-adopting the existing tag/version rather
  # than computing a new one (which would orphan the draft and its uploaded
  # assets). Chosen over failing the build because the fan-out build phases
  # are already idempotent (asset upload uses --clobber; publish re-verifies),
  # so resuming is safe and avoids manual tag cleanup for a routine retry.
  #
  # Only auto-resume on a manual trigger (or an explicit override): a cron
  # tick that resumes an unpublishable draft would otherwise re-fan-out and
  # fail every time it runs, forever, burning two release builds per tick
  # with no bound. A scheduled run that sees a stranded draft stops instead
  # and points at how to recover it — see "Abandoning a draft release" in
  # tools/changelog/docs/buildkite-release-setup.md.
  local head_sha
  head_sha="$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")"
  if [[ -n "${LAST_DRAFT_SHA}" && "${head_sha}" == "${LAST_DRAFT_SHA}" ]]; then
    if ! is_manual && [[ "${CHANGELOG_RESUME_DRAFT:-}" != "1" ]]; then
      die "release ${LAST_DRAFT_TAG} exists as an unpublished draft for this commit (${head_sha:0:12}), and this is a scheduled trigger — refusing to auto-resume it forever on every cron tick. To resume it, either re-trigger this pipeline manually (ui/api), or set CHANGELOG_RESUME_DRAFT=1 on a scheduled build. To abandon it instead, delete the draft and its tag (see 'Abandoning a draft release' in tools/changelog/docs/buildkite-release-setup.md): gh release delete ${LAST_DRAFT_TAG} --repo ${REPO} --yes && git push origin :refs/tags/${LAST_DRAFT_TAG}"
    fi
    NEW_TAG="${LAST_DRAFT_TAG}"
    NEW_VERSION="${NEW_TAG#"${TAG_PREFIX}"}"
    log "[changelog-release] resuming existing draft release ${NEW_TAG} for ${head_sha:0:12} — skipping tag/release creation"
    meta_set "${META_TAG_KEY}" "${NEW_TAG}"
    if command -v buildkite-agent &>/dev/null; then
      # The build fragment may already be uploaded in THIS build (e.g. the
      # prepare job itself was retried after uploading it the first time
      # around) — Buildkite step keys are unique per build, so re-uploading
      # would fail on a duplicate-key collision. Only upload if the publish
      # step (the last one in the fragment) isn't already present.
      if buildkite-agent step get id --step "changelog-release-publish" &>/dev/null; then
        log "[changelog-release] build phases already present in this build — not re-uploading"
      else
        log "[changelog-release] uploading build phases for ${NEW_TAG}"
        buildkite-agent pipeline upload .buildkite/pipeline-changelog-release-builds.yml
      fi
    fi
    log "[changelog-release] prepare done — resumed draft ${NEW_TAG}; build phases will attach remaining assets"
    return 0
  fi

  local skip_reason
  skip_reason="$(should_skip)" || true
  if [[ -n "${skip_reason}" ]]; then
    echo "${skip_reason}"
    exit 0  # no tag published to meta-data → the build phases skip too
  fi

  compute_next_version
  log "[changelog-release] ${CUR_VERSION} -> ${NEW_VERSION} (tag ${NEW_TAG})"

  # Tag the existing commit on main (no bump commit is created). Pushing a tag
  # (unlike a commit to main) needs no branch-protection bypass.
  local release_sha
  release_sha="$(git rev-parse HEAD 2>/dev/null || echo "${BUILDKITE_COMMIT:-}")"
  [[ -n "${release_sha}" ]] || die "could not resolve the commit to release/tag"

  # ── point of no return: tag the release commit, push the tag, create release ─
  # Re-fetch tags immediately before tagging to close the race window between the
  # initial fetch at the start of this phase and now. If the computed tag already
  # exists it was published (by this or a concurrent run) since our initial fetch;
  # fail with an actionable message rather than a raw git exit 128.
  git fetch --tags --prune --prune-tags origin \
    || die "git fetch --tags failed before tagging; aborting to avoid a duplicate-tag collision"
  if git rev-parse --verify "refs/tags/${NEW_TAG}" &>/dev/null; then
    local existing_sha
    existing_sha="$(git rev-list -n 1 "${NEW_TAG}" 2>/dev/null || echo '(unknown)')"
    die "computed tag ${NEW_TAG} already exists on remote (at commit ${existing_sha:0:12}); our HEAD is ${release_sha:0:12}. The version resolver produced a stale or already-published result — re-run the pipeline to retry with a fresh tag set."
  fi
  # The cleanup trap deletes the tag if this phase dies before the release is
  # created (the window guarded by TAG_PUSHED). Once the release exists, the
  # build phases attach assets to it; a build failure is recoverable by retrying
  # that job, so the tag/release are intentionally left in place.
  log "[changelog-release] tagging ${NEW_TAG} at ${release_sha:0:12}"
  git tag "${NEW_TAG}" "${release_sha}"
  LOCAL_TAG_CREATED=1
  git push origin "refs/tags/${NEW_TAG}" \
    || die "tag push rejected for ${NEW_TAG}; the agent's git credentials may not be able to push to ${REPO}."
  TAG_PUSHED=1

  log "[changelog-release] generating release notes for ${NEW_TAG}"
  NOTES_FILE="$(mktemp /tmp/changelog-release-notes-XXXXXX.md)"
  if [[ -n "${LAST_TAG}" ]]; then
    # Ensure full history for git log — a shallow clone silently truncates the
    # commit range returned by changelog, including on manual (ui/api) triggers
    # where the change-detection unshallow inside should_skip() is skipped.
    if git rev-parse --is-shallow-repository 2>/dev/null | grep -q true; then
      echo "[changelog-release] unshallowing repo for changelog"
      git fetch --unshallow origin 2>/dev/null || true
    fi
    bin/changelog \
      --project tools/changelog/PROJECT.yaml \
      --from "${LAST_TAG}" \
      --to "${NEW_TAG}" \
      --repo "${REPO}" \
      --enrich \
      > "${NOTES_FILE}"
  else
    printf 'Initial changelog release.\n' > "${NOTES_FILE}"
  fi

  # Created as a DRAFT: a published release must mean "every expected asset is
  # present and verified" (see phase_publish). Publishing up front, before any
  # binary exists, would advertise a resolvable-but-unfetchable version to
  # rules_multitool consumers for the whole build window. The draft is
  # published only by phase_publish, after all build phases have uploaded and
  # every REQUIRED_ASSETS entry has been checksum-verified.
  log "[changelog-release] creating GitHub Release ${NEW_TAG} (draft)"
  gh release create "${NEW_TAG}" --repo "${REPO}" --draft \
    --title "changelog ${NEW_VERSION}" \
    --notes-file "${NOTES_FILE}"

  # Hand the tag to the parallel build phases.
  meta_set "${META_TAG_KEY}" "${NEW_TAG}"
  TAG_PUSHED=0  # release created; stop guarding the tag. A build/publish
                # failure from here on intentionally leaves the tag + draft in
                # place (recoverable by retrying a phase, or inspectable as a
                # failed draft) rather than deleting them.

  # Fan out the build phases. The linux/darwin/publish steps live in a
  # separate fragment file and are only injected into the running build now
  # that we know a release is being cut. On a no-release run this block is
  # never reached, so those steps never appear in the build and no bazel
  # builds are wasted.
  if command -v buildkite-agent &>/dev/null; then
    log "[changelog-release] uploading build phases for ${NEW_TAG}"
    buildkite-agent pipeline upload .buildkite/pipeline-changelog-release-builds.yml
  fi
  log "[changelog-release] prepare done — ${NEW_TAG} created as a draft; build phases will attach assets, then publish will verify and publish"
}

# resolve_release_tag — set NEW_TAG/NEW_VERSION from the tag prepare published to
# meta-data (or a CHANGELOG_RELEASE_TAG override for manual recovery). Exits 0
# when there is no tag — prepare skipped this run, so the build phase is a no-op.
resolve_release_tag() {
  NEW_TAG="${CHANGELOG_RELEASE_TAG:-$(meta_get "${META_TAG_KEY}")}"
  if [[ -z "${NEW_TAG}" ]]; then
    echo "[changelog-release] no release tag from the prepare phase (it skipped or did not run) — nothing to do"
    exit 0
  fi
  NEW_VERSION="${NEW_TAG#"${TAG_PREFIX}"}"
}

phase_linux() {
  [[ "$(uname -s)" == "Linux" ]] || die "linux phase landed on $(uname -s); the step must target an os=linux agent (see agents: in .buildkite/pipeline-changelog-release.yml)"

  echo "[changelog-release] agent: $(uname -a)"
  resolve_release_tag

  log "[changelog-release] building Linux asset for ${NEW_TAG}"
  STAGE="$(mktemp -d)"

  local gnu_path
  gnu_path="$(build_native_bazel)"
  stage_asset "${gnu_path}" "${ASSET_PREFIX}-x86_64-unknown-linux-gnu"

  upload_release_assets
  log "[changelog-release] linux phase done — Linux asset attached to ${NEW_TAG}"
}

phase_darwin() {
  [[ "$(uname -s)" == "Darwin" ]] || die "darwin phase must run on a macOS agent (got $(uname -s)); the step must target an os=darwin agent (see agents: in .buildkite/pipeline-changelog-release.yml)"

  echo "[changelog-release] agent: $(uname -a)"
  resolve_release_tag

  log "[changelog-release] building macOS assets for ${NEW_TAG}"

  STAGE="$(mktemp -d)"

  # Native arm64 via bazel (matches how mono builds changelog).
  local arm_path
  arm_path="$(build_native_bazel)"
  stage_asset "${arm_path}" "${ASSET_PREFIX}-aarch64-apple-darwin"

  # x86_64 via cargo cross — Apple's toolchain builds both arches natively.
  local x86_path
  if x86_path="$(build_cross_cargo x86_64-apple-darwin)"; then
    stage_asset "${x86_path}" "${ASSET_PREFIX}-x86_64-apple-darwin"
  else
    echo "[changelog-release] WARNING: darwin x86_64 build failed; shipping arm64 only"
  fi

  upload_release_assets
  log "[changelog-release] darwin phase done — macOS assets attached to ${NEW_TAG}"
}

# verify_asset NAME — download NAME + its .sha256 sidecar from the release into
# STAGE and confirm the downloaded binary's checksum matches the sidecar. This
# verifies what actually landed on GitHub, not just what a build phase staged
# locally — the whole point is to catch a truncated/corrupted upload before
# publish, not to re-trust the build phase's own local computation.
verify_asset() {
  local name="$1"
  log "[changelog-release] verifying ${name}"
  gh release download "${NEW_TAG}" --repo "${REPO}" --dir "${STAGE}" --clobber \
    --pattern "${name}" --pattern "${name}.sha256" \
    || die "failed to download ${name} (or its .sha256 sidecar) from ${NEW_TAG} for verification — leaving release as a draft for inspection"
  [[ -f "${STAGE}/${name}" && -f "${STAGE}/${name}.sha256" ]] \
    || die "${name} or its .sha256 sidecar did not download from ${NEW_TAG} — leaving release as a draft for inspection"

  local expected actual
  expected="$(awk '{print $1}' "${STAGE}/${name}.sha256")"
  actual="$(_sha256 "${STAGE}/${name}")"
  [[ -n "${expected}" && "${expected}" == "${actual}" ]] \
    || die "checksum mismatch for ${name} on ${NEW_TAG}: sidecar says ${expected:-<empty>}, downloaded binary hashes to ${actual} — the asset is corrupt or truncated; leaving release as a draft for inspection"
}

# phase_publish — verify every REQUIRED_ASSETS entry (and any present
# OPTIONAL_ASSETS entry) is on the release and checksum-correct, then flip the
# draft to published. This is the ONLY place that publishes: a missing or
# mismatched required asset dies here, which (via the EXIT trap) leaves the
# draft and its assets in place for inspection rather than publishing a
# partial release. Runs after linux/darwin via `depends_on` in the build
# fragment, so it observes the final state of all uploads.
phase_publish() {
  echo "[changelog-release] agent: $(uname -a)"
  resolve_release_tag

  log "[changelog-release] fetching asset list for ${NEW_TAG}"
  local remote_assets
  remote_assets="$(gh release view "${NEW_TAG}" --repo "${REPO}" --json assets --jq '.assets[].name')" \
    || die "could not list assets for ${NEW_TAG} — leaving release as a draft"

  local name missing=()
  for name in "${REQUIRED_ASSETS[@]}"; do
    grep -qFx "${name}" <<<"${remote_assets}" || missing+=("${name}")
    grep -qFx "${name}.sha256" <<<"${remote_assets}" || missing+=("${name}.sha256")
  done
  if (( ${#missing[@]} > 0 )); then
    die "release ${NEW_TAG} is missing required assets: ${missing[*]} — leaving release as a draft for inspection (this is expected if a build phase failed or hasn't run yet)"
  fi

  STAGE="$(mktemp -d)"

  local to_verify=("${REQUIRED_ASSETS[@]}")
  for name in "${OPTIONAL_ASSETS[@]}"; do
    if grep -qFx "${name}" <<<"${remote_assets}"; then
      if grep -qFx "${name}.sha256" <<<"${remote_assets}"; then
        to_verify+=("${name}")
      else
        die "optional asset ${name} is present on ${NEW_TAG} without its .sha256 sidecar — leaving release as a draft for inspection"
      fi
    else
      log "[changelog-release] optional asset ${name} not present — skipping (not release-blocking)"
    fi
  done

  for name in "${to_verify[@]}"; do
    verify_asset "${name}"
  done

  log "[changelog-release] all required assets present and checksum-verified for ${NEW_TAG} — publishing"
  gh release edit "${NEW_TAG}" --repo "${REPO}" --draft=false \
    || die "verification passed but publishing ${NEW_TAG} failed — release remains a draft; retry the publish phase"
  log "[changelog-release] published ${NEW_TAG}"
}

# ── entrypoint ────────────────────────────────────────────────────────────────

main() {
  local phase="${1:-}"
  trap cleanup EXIT
  case "${phase}" in
    prepare) phase_prepare ;;
    linux)   phase_linux ;;
    darwin)  phase_darwin ;;
    publish) phase_publish ;;
    *) die "usage: $0 <prepare|linux|darwin|publish>" ;;
  esac
}

main "$@"
