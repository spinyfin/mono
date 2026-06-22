#!/usr/bin/env bash
# checks.sh — CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.).
# Scoped to what changed — checkleft classifies the environment automatically.
# --all is manual-only, for catching/fixing pre-existing violations.
#
# checkleft is invoked via repobin (bin/checkleft) rather than `bazel run` so
# that the binary runs with the repository root as its working directory.
# `bazel run` sets the process cwd to the Bazel runfiles tree, which causes
# checkleft to miss CHECKS.* config files; repobin builds the target and then
# execs the binary directly, preserving the caller's cwd.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [checks] running checks"
# EXPERIMENT: --all scans the full repo so pre-existing violations surface as
# failures in the progress UI — needed to test how the UI renders red ✗ entries.
# Normal CI uses the default change-scoped run (no --all).
CLICOLOR_FORCE=1 bin/checkleft run --all --show-progress=true
