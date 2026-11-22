#!/usr/bin/env bash
# integrity-checkleft.sh — run checkleft over the entire repo (--all).
#
# Unlike checks.sh in the PR pipeline (which scopes to changed paths), this
# step checks the full repo to surface pre-existing violations that per-PR
# diff-scoped runs would miss.
#
# checkleft is invoked via repobin so that the binary runs with the repository
# root as its working directory — same rationale as checks.sh.
set -euo pipefail

# This step may execute its bazel actions on remote workers; ci-env.sh
# decides whether it does (see remote-execution.sh). It qualifies because
# nothing it passes to bazel is a credential. checkleft and
# repobin build the tools they then run, which downloads those outputs as usual.
#
# Do not set CI_BAZEL_VERIFY_ONLY: same as checks.sh — checkleft's bazel uses
# CI_BAZEL_FLAGS without ci-strip, and tool-install must match.
CI_REMOTE_EXECUTION_ALLOWED=1
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [integrity-checkleft] running checkleft --all"
CLICOLOR_FORCE=1 bin/checkleft run --all
