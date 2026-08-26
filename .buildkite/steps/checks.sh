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
# format/oxc and lint/oxc need npx on PATH; Linux GCE agents do not ship Node.
source "$(dirname "${BASH_SOURCE[0]}")/ensure-node.sh"
ensure_npx

echo "--- [checks] running checks"
# checkleft already defaults --show-progress off outside an interactive
# terminal (see should_show_progress in tools/checkleft/src/main.rs), which
# covers CI. Leave it on auto rather than forcing it, so the log stays
# readable and the automated log-excerpt collector can find the failure.
CLICOLOR_FORCE=1 bin/checkleft run
