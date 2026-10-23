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
# Fail closed: on a pull request build, a fetch or merge-base failure is fatal,
# because skipping would leave the checked-in baseline (which the same pull
# request can regenerate) as the only comparison. The one successful path
# without a staged file is the baseline being positively absent at the merge
# base, i.e. this pull request introduces the schema. Push and merge-queue
# builds check the checked-in baseline alone (the pull request was already
# checked).
set -euo pipefail

baseline="tools/boss/protocol-spoke/compat/baseline/boss/spoke/v1/spoke.proto"
staged="tools/boss/protocol-spoke/compat/merge_base/boss/spoke/v1/spoke.proto"

pull_request="${BUILDKITE_PULL_REQUEST:-false}"
base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-}"
if [[ "${pull_request}" == "false" || -z "${base_branch}" ]]; then
  echo "[spoke-proto] not a pull request build; checking the checked-in baseline only"
  exit 0
fi

fail() {
  echo "[spoke-proto] ERROR: cannot stage the merge-base baseline: $1" >&2
  exit 1
}

git fetch --quiet --no-tags origin "${base_branch}" || fail "could not fetch origin/${base_branch}"
if ! merge_base="$(git merge-base HEAD FETCH_HEAD 2>/dev/null)"; then
  # A shallow checkout may not reach the merge base yet; deepen once.
  if [[ "$(git rev-parse --is-shallow-repository)" == "true" ]]; then
    git fetch --quiet --no-tags --unshallow origin || fail "could not deepen the shallow checkout"
    git fetch --quiet --no-tags origin "${base_branch}" || fail "could not fetch origin/${base_branch}"
  fi
  merge_base="$(git merge-base HEAD FETCH_HEAD 2>/dev/null)" || fail "no merge base with origin/${base_branch}"
fi

if ! git cat-file -e "${merge_base}:${baseline}" 2>/dev/null; then
  echo "[spoke-proto] no baseline on merge base ${merge_base:0:12}; this pull request introduces the schema"
  exit 0
fi

mkdir -p "$(dirname "${staged}")"
git show "${merge_base}:${baseline}" >"${staged}" || fail "could not read the baseline at ${merge_base:0:12}"
echo "[spoke-proto] staged the baseline from merge base ${merge_base:0:12}"
