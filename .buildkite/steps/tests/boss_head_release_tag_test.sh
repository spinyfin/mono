#!/bin/bash
#
# Tests for the cheap already-tagged skip boss-release.sh runs before
# ci-env.sh builds //tools/release.
#
# Fixture files are written with printf, not here-documents: bash 3.2 (the
# macOS /bin/bash) backs here-documents with files under /tmp regardless of
# TMPDIR, and the sandbox denies that.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../fetch-boss-tags.sh
source "${workspace}/.buildkite/steps/fetch-boss-tags.sh"
# shellcheck source=../boss-head-release-tag.sh
source "${workspace}/.buildkite/steps/boss-head-release-tag.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

assert_eq() {
  local expected="$1" actual="$2" msg="$3"
  if [[ "$expected" != "$actual" ]]; then
    fail "${msg}: expected '${expected}', got '${actual}'"
  fi
}

tmpdir="$(mktemp -d "${TEST_TMPDIR:-/tmp}/boss-head-release-tag.XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT

# ---------------------------------------------------------------------------
# Config loading from tools/boss/release.toml.
# ---------------------------------------------------------------------------

if ! ci_boss_load_release_config "${workspace}/tools/boss/release.toml"; then
  fail "expected tools/boss/release.toml to parse"
fi
assert_eq "spinyfin/mono" "$CI_BOSS_RELEASE_REPO" "release.toml repo"
assert_eq "boss-v" "$CI_BOSS_RELEASE_TAG_PREFIX" "release.toml tag_prefix"
assert_eq "1.0" "$CI_BOSS_RELEASE_MAJOR_MINOR" "release.toml major_minor"

if ci_boss_load_release_config "${tmpdir}/does-not-exist.toml"; then
  fail "missing release.toml should fail config load"
fi

printf '%s\n' 'repo = "x/y"' 'tag_prefix = "v"' '[version]' 'scheme = "alpha-counter"' \
  > "${tmpdir}/alpha.toml"
if ci_boss_load_release_config "${tmpdir}/alpha.toml"; then
  fail "non-patch-counter scheme should fail config load"
fi

# Re-load real config for the remaining unit checks.
ci_boss_load_release_config "${workspace}/tools/boss/release.toml"

# ---------------------------------------------------------------------------
# Version-family matching agrees with tools/release via the shared corpus.
# ---------------------------------------------------------------------------

corpus="${workspace}/tools/release/src/testdata/boss-version-family-cases.txt"
checked=0
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -n "$line" && "$line" != \#* ]] || continue
  tag="${line%% *}"
  want="${line##* }"
  status=0
  ci_boss_release_tag_matches "$tag" || status=$?
  if [[ "$want" == "yes" && "$status" -ne 0 ]]; then
    fail "expected ${tag} to match version family"
  elif [[ "$want" == "no" && "$status" -eq 0 ]]; then
    fail "expected ${tag} not to match version family"
  fi
  checked=$((checked + 1))
done < "$corpus"
if (( checked == 0 )); then
  fail "shared corpus ${corpus} had no cases"
fi

# Skip message matches prepare's AlreadyReleased log line.
assert_eq \
  "release skipped: HEAD is already boss-v1.0.1102 — re-releasing the same commit is a no-op" \
  "$(ci_boss_already_released_skip_message "boss-v1.0.1102")" \
  "already-released skip message"

# ---------------------------------------------------------------------------
# Fixture origin + agent checkout (Buildkite-style tagless fetch).
# ---------------------------------------------------------------------------

git init --bare "${tmpdir}/origin.git" >/dev/null
git clone "${tmpdir}/origin.git" "${tmpdir}/work" >/dev/null 2>&1
git -C "${tmpdir}/work" config user.email "test@example.com"
git -C "${tmpdir}/work" config user.name "Test"

mkdir -p "${tmpdir}/work/tools/boss"
cp "${workspace}/tools/boss/release.toml" "${tmpdir}/work/tools/boss/release.toml"
printf 'v1\n' > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" add file.txt tools/boss/release.toml
git -C "${tmpdir}/work" commit -m "commit 1" >/dev/null
git -C "${tmpdir}/work" tag boss-v1.0.1
git -C "${tmpdir}/work" tag other-v1.0.0
git -C "${tmpdir}/work" push origin HEAD:main --tags >/dev/null 2>&1

printf 'v2\n' > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" commit -am "commit 2" >/dev/null
c2_sha="$(git -C "${tmpdir}/work" rev-parse HEAD)"
git -C "${tmpdir}/work" tag boss-v1.0.2
git -C "${tmpdir}/work" tag boss-v1.0.stale
git -C "${tmpdir}/work" push origin HEAD:main --tags >/dev/null 2>&1

printf 'v3\n' > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" commit -am "commit 3 untagged" >/dev/null
c3_sha="$(git -C "${tmpdir}/work" rev-parse HEAD)"
git -C "${tmpdir}/work" push origin HEAD:main >/dev/null 2>&1

printf 'v4\n' > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" commit -am "commit 4 overflow tag" >/dev/null
c4_sha="$(git -C "${tmpdir}/work" rev-parse HEAD)"
git -C "${tmpdir}/work" tag boss-v1.0.18446744073709551616
git -C "${tmpdir}/work" push origin HEAD:main --tags >/dev/null 2>&1

clone_at() {
  local dest="$1" sha="$2"
  git clone -b main "${tmpdir}/origin.git" "$dest" >/dev/null 2>&1
  git -C "$dest" fetch --no-tags origin "$sha" >/dev/null 2>&1
  git -C "$dest" checkout -f "$sha" >/dev/null 2>&1
}

# Tagged HEAD after fetch: highest matching tag, ignoring boss-v1.0.stale.
clone_at "${tmpdir}/tagged" "$c2_sha"
ci_fetch_boss_tags "${tmpdir}/tagged" origin >/dev/null 2>&1
got="$(ci_boss_head_release_tag "${tmpdir}/tagged" || true)"
assert_eq "boss-v1.0.2" "$got" "tagged HEAD after fetch"

# Untagged HEAD: parent is tagged, HEAD is not.
clone_at "${tmpdir}/untagged" "$c3_sha"
ci_fetch_boss_tags "${tmpdir}/untagged" origin >/dev/null 2>&1
if tag="$(ci_boss_head_release_tag "${tmpdir}/untagged")"; then
  fail "untagged HEAD should not report a release tag, got ${tag}"
fi

# Overflow-only HEAD: > u64::MAX tag is ignored.
clone_at "${tmpdir}/overflow" "$c4_sha"
ci_fetch_boss_tags "${tmpdir}/overflow" origin >/dev/null 2>&1
if tag="$(ci_boss_head_release_tag "${tmpdir}/overflow")"; then
  fail "u64-overflow tag should be ignored, got ${tag}"
fi

# Two matching tags on HEAD: pick the highest counter.
git -C "${tmpdir}/work" tag boss-v1.0.3 "$c2_sha"
git -C "${tmpdir}/work" push origin "refs/tags/boss-v1.0.3" >/dev/null 2>&1
ci_fetch_boss_tags "${tmpdir}/tagged" origin >/dev/null 2>&1
got="$(ci_boss_head_release_tag "${tmpdir}/tagged" || true)"
assert_eq "boss-v1.0.3" "$got" "highest matching tag on HEAD"

# Large u64 counter comparison without signed 64-bit bash overflow.
git -C "${tmpdir}/work" tag boss-v1.0.18446744073709551615 "$c4_sha"
git -C "${tmpdir}/work" push origin "refs/tags/boss-v1.0.18446744073709551615" >/dev/null 2>&1
ci_fetch_boss_tags "${tmpdir}/overflow" origin >/dev/null 2>&1
got="$(ci_boss_head_release_tag "${tmpdir}/overflow" || true)"
assert_eq "boss-v1.0.18446744073709551615" "$got" "u64::MAX tag selected over overflow tag"

# ---------------------------------------------------------------------------
# bazel + gh stubs: verify published skip vs draft/resume-draft/error fallthrough.
# ---------------------------------------------------------------------------

stub_bin="${tmpdir}/stub-bin"
mkdir -p "$stub_bin"
printf '%s\n' '#!/bin/bash' 'echo "bazel invoked: $*" >> "${TRACE_FILE}"' 'exit 97' > "${stub_bin}/bazel"
chmod +x "${stub_bin}/bazel"

printf '%s\n' \
  '#!/bin/bash' \
  'echo "gh invoked: $*" >> "${TRACE_FILE}"' \
  'if [[ "${STUB_GH_IS_DRAFT:-false}" == "error" ]]; then' \
  '  exit 1' \
  'fi' \
  'printf "%s\n" "${STUB_GH_IS_DRAFT:-false}"' \
  > "${stub_bin}/gh"
chmod +x "${stub_bin}/gh"

export PATH="${stub_bin}:$PATH"

TRACE_FILE="${tmpdir}/trace"
export TRACE_FILE
: > "$TRACE_FILE"

steps_dir="${tmpdir}/steps"
mkdir -p "$steps_dir"
cp "${workspace}/.buildkite/steps/boss-release.sh" "$steps_dir/"
cp "${workspace}/.buildkite/steps/fetch-boss-tags.sh" "$steps_dir/"
cp "${workspace}/.buildkite/steps/boss-head-release-tag.sh" "$steps_dir/"
printf '%s\n' '#!/bin/bash' 'echo prepare-invoked >> "${TRACE_FILE}"' 'exit 0' > "${tmpdir}/fake-release"
chmod +x "${tmpdir}/fake-release"
printf '%s\n' \
  'echo "ci-env stub sourced" >> "${TRACE_FILE}"' \
  'mkdir -p bin' \
  "cp '${tmpdir}/fake-release' bin/release" \
  > "${steps_dir}/ci-env.sh"

export BOSS_SHAKE_APP_ID=test
export BOSS_SHAKE_INSTALLATION_ID=test
export BOSS_SHAKE_PRIVATE_KEY_PEM=test

run_stub_step() {
  local repo="$1"
  (
    cd "$repo"
    bash "${steps_dir}/boss-release.sh"
  )
}

# 1. Scheduled + published release (isDraft=false): skips before ci-env/bazel.
: > "$TRACE_FILE"
if ! out="$(BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=false run_stub_step "${tmpdir}/tagged")"; then
  fail "scheduled published tagged boss-release.sh should exit 0"
fi
assert_eq \
  "release skipped: HEAD is already boss-v1.0.3 — re-releasing the same commit is a no-op" \
  "$out" \
  "scheduled published skip output"
if grep -q 'ci-env stub sourced' "$TRACE_FILE"; then
  fail "scheduled published should not source ci-env.sh: $(cat "$TRACE_FILE")"
fi
if grep -q 'bazel invoked' "$TRACE_FILE"; then
  fail "scheduled published invoked bazel: $(cat "$TRACE_FILE")"
fi

# 2. UI trigger + published release (isDraft=false): also skips before ci-env/bazel.
: > "$TRACE_FILE"
if ! out="$(BUILDKITE_SOURCE=ui STUB_GH_IS_DRAFT=false run_stub_step "${tmpdir}/tagged")"; then
  fail "ui published tagged boss-release.sh should exit 0"
fi
assert_eq \
  "release skipped: HEAD is already boss-v1.0.3 — re-releasing the same commit is a no-op" \
  "$out" \
  "ui published skip output"
if grep -q 'ci-env stub sourced' "$TRACE_FILE"; then
  fail "ui published should not source ci-env.sh: $(cat "$TRACE_FILE")"
fi

# 3. Scheduled + stranded draft (isDraft=true): reaches prepare.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=true run_stub_step "${tmpdir}/tagged"); then
  fail "scheduled draft tagged boss-release.sh should exit 0 through stub prepare"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "scheduled draft tagged HEAD should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi

# 4. UI trigger + stranded draft (isDraft=true): reaches prepare and resumes.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=ui STUB_GH_IS_DRAFT=true run_stub_step "${tmpdir}/tagged"); then
  fail "ui draft tagged boss-release.sh should exit 0 through stub prepare"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "ui draft tagged HEAD should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi

# 5. Scheduled + published + RELEASE_RESUME_DRAFT=1: reaches prepare.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=false RELEASE_RESUME_DRAFT=1 run_stub_step "${tmpdir}/tagged"); then
  fail "RELEASE_RESUME_DRAFT=1 should exit 0 through stub prepare"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "RELEASE_RESUME_DRAFT=1 should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi

# 6. Scheduled + published + CHECKLEFT_RESUME_DRAFT=1 fallback: reaches prepare.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=false CHECKLEFT_RESUME_DRAFT=1 run_stub_step "${tmpdir}/tagged"); then
  fail "CHECKLEFT_RESUME_DRAFT=1 should exit 0 through stub prepare"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "CHECKLEFT_RESUME_DRAFT=1 should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi

# 7. Scheduled + missing release / gh error: reaches prepare.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=error run_stub_step "${tmpdir}/tagged"); then
  fail "gh error should fall through to stub prepare"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "gh error should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi

# 8. Untagged HEAD: reaches prepare.
: > "$TRACE_FILE"
if ! (BUILDKITE_SOURCE=schedule STUB_GH_IS_DRAFT=false run_stub_step "${tmpdir}/untagged"); then
  fail "untagged boss-release.sh should exit 0 through stub prepare"
fi
if ! grep -q 'ci-env stub sourced' "$TRACE_FILE"; then
  fail "untagged HEAD should source ci-env.sh: $(cat "$TRACE_FILE")"
fi
if ! grep -q 'prepare-invoked' "$TRACE_FILE"; then
  fail "untagged HEAD should invoke bin/release prepare: $(cat "$TRACE_FILE")"
fi
if grep -q 'bazel invoked' "$TRACE_FILE"; then
  fail "untagged path invoked bazel: $(cat "$TRACE_FILE")"
fi

# Ordering: fetch + already-tagged skip must precede sourcing ci-env.sh, and
# prepare must still run after that skip.
step_sh="${workspace}/.buildkite/steps/boss-release.sh"
fetch_line="$(grep -nF 'if ! ci_fetch_boss_tags; then' "$step_sh" | head -1 | cut -d: -f1)"
skip_pat='tag="$(ci_boss_head_release_tag)"'
skip_line="$(grep -nF "$skip_pat" "$step_sh" | head -1 | cut -d: -f1)"
env_line="$(grep -nF '/ci-env.sh"' "$step_sh" | head -1 | cut -d: -f1)"
prepare_pat='TAG="$(bin/release prepare'
prepare_line="$(grep -nF "$prepare_pat" "$step_sh" | head -1 | cut -d: -f1)"
if [[ -z "$fetch_line" || -z "$skip_line" || -z "$env_line" || -z "$prepare_line" ]]; then
  fail "expected fetch, skip, ci-env source, and prepare in boss-release.sh"
elif (( fetch_line >= skip_line || skip_line >= env_line || env_line >= prepare_line )); then
  fail "expected fetch (${fetch_line}) < skip (${skip_line}) < ci-env (${env_line}) < prepare (${prepare_line})"
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
