#!/usr/bin/env bash
# Smoke test for rust_wasm_component: assert the .wasm.sha256 sidecar matches the
# component it describes, and that the component has a Component Model preamble.
#
# Args are the runfiles of one rust_wasm_component target (a .wasm and a
# .wasm.sha256, in either order).
set -euo pipefail

wasm=""
sha=""
for f in "$@"; do
    case "$f" in
        *.wasm.sha256) sha="$f" ;;
        *.wasm) wasm="$f" ;;
    esac
done

[ -n "$wasm" ] || {
    echo "FAIL: no .wasm in inputs: $*" >&2
    exit 1
}
[ -n "$sha" ] || {
    echo "FAIL: no .wasm.sha256 in inputs: $*" >&2
    exit 1
}

# Component preamble: 00 61 73 6d 0d 00 01 00 -> the "layer" byte (offset 6) is 1.
layer="$(od -An -tu1 -j6 -N1 "$wasm" | tr -d '[:space:]')"
if [ "$layer" != "1" ]; then
    echo "FAIL: $wasm is not a Component Model component (layer byte = $layer, want 1)" >&2
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$wasm" | cut -d' ' -f1)"
else
    actual="$(shasum -a 256 "$wasm" | awk '{print $1}')"
fi
expected="$(tr -d '[:space:]' <"$sha")"

if [ "$actual" != "$expected" ]; then
    echo "FAIL: sha256 sidecar mismatch" >&2
    echo "  sidecar:  $expected" >&2
    echo "  computed: $actual" >&2
    exit 1
fi

echo "OK: component is valid and sha256 sidecar matches ($actual)"
