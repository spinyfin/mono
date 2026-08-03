#!/usr/bin/env bash
# Fails if the checked-in Sources/EngineBinaryName.swift has drifted from
# the engine_binary_name_swift genrule output (the single source of truth
# derived from //tools/boss/engine/core:engine_binary.bzl). Compares file
# contents via bash builtins rather than diff/cmp: neither is on PATH in
# the hermetic test sandbox (confirmed locally: "cmp: command not found").
# A trailing sentinel byte around each $(cat ...) capture stops the shell
# from stripping trailing newlines, so the comparison stays byte-exact.
set -euo pipefail

checked_in="$1"
generated="$2"

checked_in_contents="$(cat "$checked_in"; echo x)"
generated_contents="$(cat "$generated"; echo x)"

if [[ "$checked_in_contents" != "$generated_contents" ]]; then
  echo "tools/boss/app-macos/Sources/EngineBinaryName.swift is out of date." >&2
  echo "Regenerate it with:" >&2
  echo "  bazel build //tools/boss/app-macos:engine_binary_name_swift && \\" >&2
  echo "  cp bazel-bin/tools/boss/app-macos/EngineBinaryName.swift tools/boss/app-macos/Sources/EngineBinaryName.swift && \\" >&2
  echo "  chmod 644 tools/boss/app-macos/Sources/EngineBinaryName.swift" >&2
  exit 1
fi
