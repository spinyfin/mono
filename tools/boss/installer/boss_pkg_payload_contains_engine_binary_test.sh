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

repobin_path="$(dirname "$engine_path")/repobin"
# Inspect the packaged mode bits, not access(X_OK): seatbelt denies execution
# of binaries inside a declared directory artifact even when their mode is
# 0555. `find -perm -111` is in the hermetic test runtime's curated tool set
# (`stat` is not) and matches only a regular file with every execute bit set.
if [[ ! -f "$repobin_path" ]] \
  || [[ -z "$(find "$repobin_path" -maxdepth 0 -type f -perm -111)" ]]; then
  echo "expected executable worker tool dispatcher at: $repobin_path" >&2
  exit 1
fi
