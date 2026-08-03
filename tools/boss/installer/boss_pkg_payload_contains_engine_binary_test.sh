#!/usr/bin/env bash
# Asserts the real staged Boss.app payload (not a tempdir mock) contains a
# regular file for the engine binary at the path engine_binary.bzl says it
# should be bundled at. Fails on drift between the binary name
# (ENGINE_BINARY_NAME) or the bundle directory (ENGINE_BINARY_BUNDLE_FRAGMENT)
# and what the macos_application rule actually produces.
set -euo pipefail

engine_path="$1"

if [[ ! -f "$engine_path" ]]; then
  echo "expected engine binary at: $engine_path" >&2
  echo "but it is not a regular file (bundle layout drifted from engine_binary.bzl?)" >&2
  exit 1
fi
