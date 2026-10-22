#!/usr/bin/env bash
# stage-spoke-proto-merge-base.sh — stage the boss.spoke.v1 schema baseline as
# it exists on this pull request's merge base, so that
# //tools/boss/protocol-spoke:proto_breaking_test can refuse a change that is
# not backward compatible with the contract already on the base branch.
#
# Without this, a pull request could make a breaking schema change and
# regenerate the checked-in baseline to match; the test would then only be
# comparing the schema with itself. The merge-base copy is an input file of the
# test (a glob in tools/boss/protocol-spoke/BUILD.bazel), not a lookup the test
# performs, so the result stays a pure function of its inputs and cacheable.
#
# Best effort by design: any problem here is reported and skipped, never fatal,
# because the checked-in baseline is still enforced without it. Only pull
# request builds stage anything; push and merge-queue builds check the
# checked-in baseline alone (the pull request was already checked).
set -uo pipefail

baseline="tools/boss/protocol-spoke/compat/baseline/boss/spoke/v1/spoke.proto"
staged="tools/boss/protocol-spoke/compat/merge_base/boss/spoke/v1/spoke.proto"

pull_request="${BUILDKITE_PULL_REQUEST:-false}"
base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-}"
if [[ "${pull_request}" == "false" || -z "${base_branch}" ]]; then
  echo "[spoke-proto] not a pull request build; checking the checked-in baseline only"
  exit 0
fi

skip() {
  echo "[spoke-proto] WARNING: not staging a merge-base baseline: $1" >&2
  exit 0
}

git fetch --quiet --no-tags origin "${base_branch}" || skip "could not fetch origin/${base_branch}"
merge_base="$(git merge-base HEAD FETCH_HEAD 2>/dev/null)" || skip "no merge base with origin/${base_branch} (shallow checkout?)"

if ! git cat-file -e "${merge_base}:${baseline}" 2>/dev/null; then
  echo "[spoke-proto] no baseline on merge base ${merge_base:0:12}; this pull request introduces the schema"
  exit 0
fi

mkdir -p "$(dirname "${staged}")" || skip "could not create the staging directory"
git show "${merge_base}:${baseline}" >"${staged}" || skip "could not read the baseline at ${merge_base:0:12}"
echo "[spoke-proto] staged the baseline from merge base ${merge_base:0:12}"
