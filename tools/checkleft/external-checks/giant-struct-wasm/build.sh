#!/usr/bin/env bash
# PROTOTYPE build step for the giant-struct-wasm sandbox-v1 guest.
#
# There is no wasm32 rust toolchain registered in MODULE.bazel, so this guest is
# built out-of-band with cargo rather than bazel. Requires the wasm32 target:
#   rustup target add wasm32-unknown-unknown
#
# Produces `giant_struct_wasm.wasm` next to this script (committed to the repo,
# loaded by the hermetic e2e test and referenced by the bundled check manifest)
# and prints its sha256 — paste it into checks/giant-struct-wasm/check.toml's
# `artifact_sha256`.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

cargo build --release --target wasm32-unknown-unknown

src="target/wasm32-unknown-unknown/release/checkleft_giant_struct_wasm.wasm"
dest="$here/giant_struct_wasm.wasm"
cp "$src" "$dest"

echo "artifact: $dest"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$dest" | awk '{print "artifact_sha256 = \""$1"\""}'
else
  shasum -a 256 "$dest" | awk '{print "artifact_sha256 = \""$1"\""}'
fi
