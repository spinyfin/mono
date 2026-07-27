#!/usr/bin/env bash
# Build and run the throwaway GhosttyKit embed harness.
set -euo pipefail
cd "$(dirname "$0")"

if [[ ! -d .local-GhosttyKit.xcframework ]]; then
  echo "missing .local-GhosttyKit.xcframework — see README.md" >&2
  exit 1
fi

echo "building ghosttykit_spike…"
swift build -c release 2>&1 | tee build.log
BIN="$(swift build -c release --show-bin-path)/ghosttykit_spike"
echo "running $BIN"
# GUI app — needs window server. Exit code is the app's terminate status.
"$BIN" 2>&1 | tee run_console.log || true
echo "--- run_out/ ---"
ls -la run_out/ 2>/dev/null || true
if [[ -f run_out/SUMMARY.txt ]]; then
  echo "=== SUMMARY ==="
  cat run_out/SUMMARY.txt
fi
