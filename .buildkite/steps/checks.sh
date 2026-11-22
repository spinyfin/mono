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

# This step may execute its bazel actions on remote workers; ci-env.sh
# decides whether it does (see remote-execution.sh). It qualifies because
# nothing it passes to bazel is a credential. checkleft and
# repobin build the tools they then run, which downloads those outputs as usual.
#
# Build bin/checkleft in opt rather than fastbuild: a fastbuild checkleft is
# much slower because its embedded wasmtime/Cranelift JIT runs unoptimized
# (~17x slower per docs/investigations/checkleft-lib-test-wasm-compile-timeout.md),
# which costs more than the re-analysis a shared fastbuild config would save.
# checkleft's own bazel invocations read CI_BAZEL_FLAGS, so the analysis cache
# stays shared.
# Do not set CI_BAZEL_VERIFY_ONLY: ci-strip is not used here.
CI_BAZEL_STEP_FLAGS="--compilation_mode=opt"
CI_REMOTE_EXECUTION_ALLOWED=1
source "$(dirname "${BASH_SOURCE[0]}")/ci-env.sh"

echo "--- [checks] running checks"
# checkleft already defaults --show-progress off outside an interactive
# terminal (see should_show_progress in tools/checkleft/src/main.rs), which
# covers CI. Leave it on auto rather than forcing it, so the log stays
# readable and the automated log-excerpt collector can find the failure.
CLICOLOR_FORCE=1 bin/checkleft run
