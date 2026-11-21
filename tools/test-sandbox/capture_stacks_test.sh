#!/bin/bash
set -euo pipefail
exec python3 "${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/test-sandbox/capture_stacks_test.py"
