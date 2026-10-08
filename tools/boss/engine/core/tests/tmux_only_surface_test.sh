#!/usr/bin/env bash
# Use the audited Python runtime supplied by the hermetic Bazel test wrapper.
set -euo pipefail
exec python3 "$TEST_SRCDIR/${TEST_WORKSPACE:-_main}/tools/boss/engine/core/tests/tmux_only_surface_test.py"
