#!/usr/bin/env bash
# spoke-protocol-test.sh — build and test //tools/boss/protocol-spoke/... on
# the OS of the agent running it. The pipeline runs this once pinned to Linux
# and once pinned to macOS arm64, so the pinned buf binaries, the hermetic
# test sandbox, loopback mTLS and the prost/tonic generator are each exercised
# on every supported platform (the general bazel-build-test job lands on
# whichever bazel-any agent is free, which does not guarantee both).
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [spoke-proto] staging merge-base schema baseline"
"$(dirname "${BASH_SOURCE[0]}")/stage-spoke-proto-merge-base.sh"

echo "--- [spoke-proto] building on $(uname -s)/$(uname -m)"
bazel build --verbose_failures -- //tools/boss/protocol-spoke/...

echo "--- [spoke-proto] testing on $(uname -s)/$(uname -m)"
bazel test --test_output=errors -- //tools/boss/protocol-spoke/...

echo "[spoke-proto] ok"
