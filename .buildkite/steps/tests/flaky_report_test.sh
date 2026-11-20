#!/bin/bash
set -euo pipefail
exec python3 "${TEST_SRCDIR}/${TEST_WORKSPACE}/.buildkite/steps/tests/flaky_report_test.py"
