#!/usr/bin/env bash
# PROTOTYPE build step for the buildifier-wasm sandbox-v1 guest.
#
# There is no wasm32 rust toolchain registered in MODULE.bazel, so this guest is
# built out-of-band with cargo rather than bazel. Requires the wasm32 target:
#   rustup target add wasm32-unknown-unknown
#
# Produces `buildifier_wasm.wasm` next to this script and prints its sha256 (to
# paste into `buildifier-wasm.toml`'s `artifact_sha256`).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

cargo build --release --target wasm32-unknown-unknown

src="target/wasm32-unknown-unknown/release/checkleft_buildifier_wasm.wasm"
dest="$here/buildifier_wasm.wasm"
cp "$src" "$dest"

echo "artifact: $dest"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$dest" | awk '{print "artifact_sha256 = \""$1"\""}'
else
  shasum -a 256 "$dest" | awk '{print "artifact_sha256 = \""$1"\""}'
fi
