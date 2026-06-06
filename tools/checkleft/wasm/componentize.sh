#!/usr/bin/env bash
# Componentize (or validate) a guest wasm artifact and emit its sha256 sidecar.
#
# Usage: componentize.sh <wasm-tools> <input.wasm> <output.wasm> <output.sha256> [adapter.wasm]
#
# rustc's wasm32-wasip2 target links via wasm-component-ld and already emits a
# Component Model component, so the common path here is validate-and-copy. The
# core-module branch (`wasm-tools component new`) is kept so the same rule also
# works for a wasm32-wasip1 core-module input, with or without a
# wasi_snapshot_preview1 adapter.
set -euo pipefail

wasm_tools="$1"
input="$2"
output="$3"
sha_out="$4"
adapter="${5:-}"

# Detect component vs. core module from the wasm preamble. Both start with the
# 4-byte magic `\0asm`; bytes 4-5 are the version and bytes 6-7 the "layer":
#   core module: 00 61 73 6d 01 00 00 00  -> byte[6] = 0x00
#   component:   00 61 73 6d 0d 00 01 00  -> byte[6] = 0x01
layer="$(od -An -tu1 -j6 -N1 "$input" | tr -d '[:space:]')"

if [ "$layer" = "1" ]; then
    # Already a component (wasm32-wasip2). Validate, then pass through unchanged.
    "$wasm_tools" validate "$input"
    cp "$input" "$output"
else
    # Core module (e.g. wasm32-wasip1). Promote it to a component.
    if [ -n "$adapter" ]; then
        "$wasm_tools" component new "$input" \
            --adapt "wasi_snapshot_preview1=$adapter" \
            -o "$output"
    else
        "$wasm_tools" component new "$input" -o "$output"
    fi
fi

# Final gate: the emitted artifact must be a valid component before we pin its
# digest into a CHECKS manifest.
"$wasm_tools" validate "$output"

# Emit the bare lowercase hex sha256 digest (no filename) that the CHECKS
# manifest pins as artifact_sha256.
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$output" | cut -d' ' -f1 >"$sha_out"
else
    shasum -a 256 "$output" | awk '{print $1}' >"$sha_out"
fi
