#!/usr/bin/env bash
# Guard: the engine must never call `gh pr merge` directly.
# Branch protection (not engine code) is the merge gate.
# Design doc: boss-ci-buildkite-pipeline-mirroring-flunge.md §R2.
set -euo pipefail

SRC="${TEST_SRCDIR}/mono/tools/boss/engine/src"

if grep -rn "gh pr merge" "${SRC}/"; then
    echo "FAIL: 'gh pr merge' found in tools/boss/engine/src/."
    echo "      Engine auto-merge must not be added without updating the CI gate design."
    exit 1
fi

echo "PASS: No 'gh pr merge' calls in engine source."
