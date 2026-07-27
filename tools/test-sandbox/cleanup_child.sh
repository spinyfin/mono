#!/bin/bash

set -euo pipefail

marker="${1:?cleanup marker is required}"
trap '' INT TERM HUP
(trap '' INT TERM HUP; sleep 30) &
descendant_pid=$!
printf '%s %s\n' "$$" "${descendant_pid}" >"${TEST_TMPDIR:?}/cleanup-child-pids"
printf '%s\n' "${marker}" >"${TEST_TMPDIR}/cleanup-child-marker"
wait "${descendant_pid}"
