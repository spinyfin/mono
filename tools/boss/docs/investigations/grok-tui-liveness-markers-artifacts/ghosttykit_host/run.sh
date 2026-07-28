#!/usr/bin/env bash
# Build and run the throwaway GhosttyKit liveness-marker harness.
# Apparatus: GhosttyKit embed only — not standalone Ghostty.app.
set -euo pipefail
cd "$(dirname "$0")"

if [[ ! -d .local-GhosttyKit.xcframework ]]; then
  echo "missing .local-GhosttyKit.xcframework — link ghosttykit-5659cef pin" >&2
  exit 1
fi

export GROK_HOME="${GROK_HOME:-/tmp/grok-liveness-spike/home}"
export SPIKE_CWD="${SPIKE_CWD:-/tmp/grok-liveness-spike/cwd}"
export SPIKE_PANE_MODE="${SPIKE_PANE_MODE:-no_alt}"

echo "building ghosttykit_liveness (mode=$SPIKE_PANE_MODE)…"
swift build -c release 2>&1 | tee build.log
BIN="$(swift build -c release --show-bin-path)/ghosttykit_liveness"
echo "running $BIN"
"$BIN" 2>&1 | tee run_console.log || true
echo "--- run_out/ ---"
ls -la run_out/ 2>/dev/null || true
if [[ -f run_out/SUMMARY.txt ]]; then
  echo "=== SUMMARY ==="
  cat run_out/SUMMARY.txt
fi
if [[ -f run_out/marker_stability.tsv ]]; then
  echo "=== marker_stability.tsv ==="
  cat run_out/marker_stability.tsv
fi
