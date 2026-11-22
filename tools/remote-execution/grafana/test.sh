#!/usr/bin/env bash
set -euo pipefail
exec python3 -B "${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/tools/remote-execution/grafana/test_grafana.py"
