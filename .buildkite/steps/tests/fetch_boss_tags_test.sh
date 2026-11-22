#!/bin/bash
#
# Tests for .buildkite/steps/fetch-boss-tags.sh, the library ci-env.sh uses
# to sync boss-v* tags before any Bazel command runs.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../fetch-boss-tags.sh
source "${workspace}/.buildkite/steps/fetch-boss-tags.sh"

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

tmpdir="$(mktemp -d "${TEST_TMPDIR:-/tmp}/fetch-boss-tags.XXXXXX")"
trap 'rm -rf "$tmpdir"' EXIT

# Set up a bare origin repo and a working clone to push commits and tags.
git init --bare "${tmpdir}/origin.git" >/dev/null
git clone "${tmpdir}/origin.git" "${tmpdir}/work" >/dev/null 2>&1
git -C "${tmpdir}/work" config user.email "test@example.com"
git -C "${tmpdir}/work" config user.name "Test"

echo "v1" > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" add file.txt
git -C "${tmpdir}/work" commit -m "commit 1" >/dev/null
git -C "${tmpdir}/work" tag boss-v1.0.1
git -C "${tmpdir}/work" tag other-v1.0.0
git -C "${tmpdir}/work" push origin HEAD:main --tags >/dev/null 2>&1

# Simulate an agent checkout that was cloned earlier and has a stale boss-v*
# tag plus an unrelated local tag.
git clone -b main "${tmpdir}/origin.git" "${tmpdir}/agent" >/dev/null 2>&1
git -C "${tmpdir}/agent" tag boss-v1.0.stale
git -C "${tmpdir}/agent" tag local-other-v1

# Advance origin: add commit 2 tagged boss-v1.0.2, move boss-v1.0.1 to commit 2
# (to test + force-update), and add another non-boss tag other-v2.0.0.
echo "v2" > "${tmpdir}/work/file.txt"
git -C "${tmpdir}/work" commit -am "commit 2" >/dev/null
c2_sha="$(git -C "${tmpdir}/work" rev-parse HEAD)"
git -C "${tmpdir}/work" tag boss-v1.0.2
git -C "${tmpdir}/work" tag -f boss-v1.0.1
git -C "${tmpdir}/work" tag other-v2.0.0
git -C "${tmpdir}/work" push origin HEAD:main --tags --force >/dev/null 2>&1

# Simulate Buildkite's tagless fetch of commit 2 on the agent.
git -C "${tmpdir}/agent" fetch --no-tags origin "$c2_sha" >/dev/null 2>&1
git -C "${tmpdir}/agent" checkout -f "$c2_sha" >/dev/null 2>&1

# Before syncing, git describe falls back to commit 1's tag (not boss-v1.0.2).
before_tag="$(git -C "${tmpdir}/agent" describe --tags --match "boss-v*" --abbrev=0)"
if [[ "$before_tag" == "boss-v1.0.2" ]]; then
  fail "pre-sync describe should not yet see boss-v1.0.2"
fi
if ! git -C "${tmpdir}/agent" rev-parse --verify "refs/tags/boss-v1.0.stale" >/dev/null 2>&1; then
  fail "expected boss-v1.0.stale to exist before sync"
fi

# Run ci_fetch_boss_tags.
ci_fetch_boss_tags "${tmpdir}/agent" origin >/dev/null 2>&1

# 1. Missing boss-v1.0.2 was fetched; git describe now resolves boss-v1.0.2 (or
#    at least not the deleted stale tag).
if ! git -C "${tmpdir}/agent" rev-parse --verify "refs/tags/boss-v1.0.2" >/dev/null 2>&1; then
  fail "expected boss-v1.0.2 to be fetched"
fi

# 2. Stale boss-v1.0.stale was pruned.
if git -C "${tmpdir}/agent" rev-parse --verify "refs/tags/boss-v1.0.stale" >/dev/null 2>&1; then
  fail "expected stale boss-v1.0.stale tag to be pruned"
fi

# 3. Moved boss-v1.0.1 was updated to commit 2.
moved_sha="$(git -C "${tmpdir}/agent" rev-parse "refs/tags/boss-v1.0.1")"
assert_eq "$c2_sha" "$moved_sha" "moved boss-v1.0.1 should be updated"

# 4. Non-boss tags were neither fetched nor pruned.
if git -C "${tmpdir}/agent" rev-parse --verify "refs/tags/other-v2.0.0" >/dev/null 2>&1; then
  fail "non-boss tag other-v2.0.0 should not have been fetched"
fi
if ! git -C "${tmpdir}/agent" rev-parse --verify "refs/tags/local-other-v1" >/dev/null 2>&1; then
  fail "local non-boss tag local-other-v1 should not have been pruned"
fi

# 5. Unreachable remote fails non-zero.
if ci_fetch_boss_tags "${tmpdir}/agent" "file://${tmpdir}/nonexistent.git" >/dev/null 2>&1; then
  fail "expected ci_fetch_boss_tags to fail on unreachable remote"
fi

# 6. ci-env.sh sources fetch-boss-tags.sh and calls ci_fetch_boss_tags before
#    its first `bazel build`.
env_sh="${workspace}/.buildkite/steps/ci-env.sh"
fetch_line="$(grep -n 'ci_fetch_boss_tags' "$env_sh" | grep -v '^[0-9]*:[[:space:]]*#' | head -n1 | cut -d: -f1)"
repobin_line="$(grep -n 'bazel build //tools/repobin:repobin' "$env_sh" | head -n1 | cut -d: -f1)"
if [[ -z "$fetch_line" || -z "$repobin_line" ]]; then
  fail "expected both ci_fetch_boss_tags and bazel build //tools/repobin:repobin in ci-env.sh"
elif (( fetch_line >= repobin_line )); then
  fail "ci_fetch_boss_tags (line ${fetch_line}) must run before bazel build //tools/repobin:repobin (line ${repobin_line})"
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
