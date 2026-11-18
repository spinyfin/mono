#!/usr/bin/env bash
# Use the audited Python runtime supplied by the hermetic Bazel test wrapper.
set -euo pipefail
exec python3 "$TEST_SRCDIR/${TEST_WORKSPACE:-_main}/tools/boss/protocol/tests/worker_roster_parity_test.py"
