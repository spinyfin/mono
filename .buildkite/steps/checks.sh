#!/usr/bin/env bash
# checks.sh — CHECKS.yaml runner (checkleft, no-generated-artifacts, etc.).
# On PR builds: scoped to changed files via --base_ref=origin/<base-branch>.
# On push-to-main builds: runs --all.
# Does not invoke jj; checkleft detects the git VCS automatically.
#
# checkleft is invoked via repobin (bin/checkleft) rather than `bazel run` so
# that the binary runs with the repository root as its working directory.
# `bazel run` sets the process cwd to the Bazel runfiles tree, which causes
# checkleft to miss CHECKS.* config files; repobin builds the target and then
# execs the binary directly, preserving the caller's cwd.
set -euo pipefail

echo "--- [checks] starting"

echo "--- [checks] installing repobin tools into bin/"
bazel build --config=ci-linux-disk-cache //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

# Determine run scope: PR builds scope to changed files; main builds run all.
if [[ "${BUILDKITE_PULL_REQUEST:-false}" != "false" ]]; then
    base_branch="${BUILDKITE_PULL_REQUEST_BASE_BRANCH:-main}"
    echo "[checks] PR build — scoping to changes against origin/${base_branch}"
    CHECKLEFT_ARGS=(run --base-ref="origin/${base_branch}")
else
    echo "[checks] push build — running all checks"
    CHECKLEFT_ARGS=(run --all)
fi

bin/checkleft "${CHECKLEFT_ARGS[@]}"

echo "[checks] ok"
