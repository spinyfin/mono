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

# 6. Exercise the jj bare-store layout with the same stale tags.
mkdir -p "${tmpdir}/jj-agent/.jj/repo/store"
git clone --bare "${tmpdir}/origin.git" "${tmpdir}/jj-agent/.jj/repo/store/git" >/dev/null 2>&1
jj_store="${tmpdir}/jj-agent/.jj/repo/store/git"
git --git-dir="$jj_store" tag -d boss-v1.0.2 >/dev/null
git --git-dir="$jj_store" tag -f boss-v1.0.1 "${c2_sha}^" >/dev/null
git --git-dir="$jj_store" tag boss-v1.0.stale "$c2_sha"
git --git-dir="$jj_store" tag local-other-v1 "$c2_sha"
git --git-dir="$jj_store" tag -d other-v2.0.0 >/dev/null
ci_fetch_boss_tags "${tmpdir}/jj-agent" origin >/dev/null 2>&1
assert_eq "$c2_sha" "$(git --git-dir="$jj_store" rev-parse refs/tags/boss-v1.0.2)" "jj missing tag fetched"
assert_eq "$c2_sha" "$(git --git-dir="$jj_store" rev-parse refs/tags/boss-v1.0.1)" "jj moved tag updated"
if git --git-dir="$jj_store" rev-parse --verify refs/tags/boss-v1.0.stale >/dev/null 2>&1; then
  fail "jj stale tag should be pruned"
fi
if git --git-dir="$jj_store" rev-parse --verify refs/tags/other-v2.0.0 >/dev/null 2>&1; then
  fail "jj unrelated remote tag should not be fetched"
fi
git --git-dir="$jj_store" rev-parse --verify refs/tags/local-other-v1 >/dev/null

# 7. Deterministic transient failures recover; persistent failures stop after
# three attempts, and success never sleeps or retries again.
check_retries() (
  local failures_before_success="$1" expected_attempts="$2" expected_sleeps="$3" expected_status="$4"
  local attempts=0 sleeps=0 status=0
  git() {
    attempts=$((attempts + 1))
    (( attempts > failures_before_success ))
  }
  sleep() { sleeps=$((sleeps + 1)); }
  ci_fetch_boss_tags "${tmpdir}/agent" origin >/dev/null 2>&1 || status=$?
  assert_eq "$expected_attempts" "$attempts" "fetch attempts"
  assert_eq "$expected_sleeps" "$sleeps" "retry sleeps"
  assert_eq "$expected_status" "$status" "fetch status"
  (( failures == 0 ))
)
check_retries 0 1 0 0 || fail "immediate success"
check_retries 2 3 2 0 || fail "transient failure recovery"
check_retries 3 3 2 1 || fail "persistent failure bound"

# 8. Check ordering against the first uncommented Bazel command, including
# the early Xcode recovery path, rather than an incidental target name.
env_sh="${workspace}/.buildkite/steps/ci-env.sh"
fetch_line="$(awk '/^[[:space:]]*if ! ci_fetch_boss_tags; then/ { print NR; exit }' "$env_sh")"
bazel_line="$(awk '/^[[:space:]]*(command[[:space:]]+)?bazel[[:space:]]/ { print NR; exit }' "$env_sh")"
if [[ -z "$fetch_line" || -z "$bazel_line" ]]; then
  fail "expected both tag sync and a Bazel invocation in ci-env.sh"
elif (( fetch_line >= bazel_line )); then
  fail "tag sync (line ${fetch_line}) must precede the first Bazel invocation (line ${bazel_line})"
fi

if ((failures > 0)); then
  echo "${failures} failure(s)" >&2
  exit 1
fi
echo "ok"
