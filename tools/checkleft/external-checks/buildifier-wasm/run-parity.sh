#!/usr/bin/env bash
# PROTOTYPE parity harness: built-in buildifier check vs the buildifier-wasm
# sandbox-v1 external check, over the same files.
#
# Requires, on the machine you run this on:
#   * `buildifier` on PATH (7+, for --format=json)
#   * a built `checkleft` binary on PATH
#   * the wasm artifact built via ./build.sh (buildifier_wasm.wasm + its sha256)
#
# This cannot run in the spike's CI worker (no buildifier, no wasm32 target). It
# documents the exact end-to-end comparison and is meant to be run by hand.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_fixture="$here/../../tests/fixtures/buildifier/malformed.bzl.fixture"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# A malformed file (buildifier will flag it) and a clean one (no findings).
cp "$repo_fixture" "$scratch/malformed.bzl"
cat > "$scratch/clean.bzl" <<'EOF'
"""A clean module docstring."""

def helper():
    """A docstring."""
    return []
EOF

wasm="$here/buildifier_wasm.wasm"
sha="$(shasum -a 256 "$wasm" | awk '{print $1}')"

# --- Config A: built-in buildifier check ------------------------------------
cat > "$scratch/CHECKS.yaml" <<'EOF'
checks:
  - id: buildifier
    config:
      buildifier_path: "buildifier"
EOF
( cd "$scratch" && checkleft run --all --format json ) > "$scratch/builtin.json" || true

# --- Config B: buildifier-wasm external check -------------------------------
cat > "$scratch/buildifier-wasm.toml" <<EOF
id = "buildifier-wasm"
mode = "artifact"
runtime = "sandbox-v1"
api_version = "v1"
artifact_path = "buildifier_wasm.wasm"
artifact_sha256 = "$sha"
[capabilities]
commands = ["buildifier", "bazel"]
EOF
cp "$wasm" "$scratch/buildifier_wasm.wasm"
cat > "$scratch/CHECKS.yaml" <<'EOF'
checks:
  - id: buildifier-wasm
    check: buildifier-wasm
    implementation: buildifier-wasm.toml
    config:
      buildifier_path: "buildifier"
EOF
( cd "$scratch" && CHECKLEFT_PROTOTYPE_SANDBOX_COMMANDS=1 checkleft run --all --format json ) \
  > "$scratch/wasm.json" || true

echo "=== built-in findings ===";  cat "$scratch/builtin.json"
echo "=== wasm findings ===";      cat "$scratch/wasm.json"

# Compare findings ignoring the (expected) differing check_id. Requires jq.
if command -v jq >/dev/null 2>&1; then
  jq -S '[.. | objects | select(has("severity")) | {severity,message,location}]' \
     "$scratch/builtin.json" > "$scratch/builtin.norm.json"
  jq -S '[.. | objects | select(has("severity")) | {severity,message,location}]' \
     "$scratch/wasm.json" > "$scratch/wasm.norm.json"
  if diff -u "$scratch/builtin.norm.json" "$scratch/wasm.norm.json"; then
    echo "PARITY OK: findings match"
  else
    echo "PARITY MISMATCH (see diff above)"; exit 1
  fi
fi
