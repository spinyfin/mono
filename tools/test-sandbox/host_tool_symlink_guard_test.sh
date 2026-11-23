#!/bin/bash
#
# Guard: a host tool that a test declares must reach the test action as a
# symlink to the host's own binary, never as a file.
#
#   host_tool_symlink_guard_test.sh <runfile> <absolute host path|--from-fingerprint>
#
# When the second argument is `--from-fingerprint`, the expected absolute host
# path is read from the sibling `fingerprint` file written by
# `host_tool_symlink` (for host-dependent tools such as the user's login shell).
#
# This is runtime_symlink_guard_test.sh for the single-tool repositories that
# host_tool_symlink generates (see README.md, "Host tools a single test
# declares"). Those tools stay out of the shared runtime and are visible to
# one or two packages only, so each such package runs this script against its
# own label.
#
# Bazel stages a declared, unresolved symlink verbatim, so the runfile points
# straight at the host path. A `repository_ctx.symlink` link is a source file:
# its runfile is linked to a path inside Bazel's own trees, or arrives as a
# plain file, and a remote executor receives a copy of the client's binary.

set -euo pipefail

if [[ "$#" -ne 2 ]]; then
  printf 'usage: %s <runfile> <absolute host path|--from-fingerprint>\n' "$0" >&2
  exit 2
fi

tool="$1"
expected="$2"

if [[ "${expected}" == "--from-fingerprint" ]]; then
  fingerprint="$(dirname "${tool}")/fingerprint"
  tool_name="$(basename "${tool}")"
  if [[ ! -f "${fingerprint}" ]]; then
    printf 'FAIL: %s not found beside %s; host_tool_symlink was not used\n' "${fingerprint}" "${tool}" >&2
    exit 1
  fi
  recorded=""
  while IFS= read -r line; do
    if [[ "${line}" == "${tool_name}="* ]]; then
      value="${line#"${tool_name}="}"
      recorded="${value%% *}"
      break
    fi
  done < "${fingerprint}"
  if [[ -z "${recorded}" || "${recorded}" != /* ]]; then
    printf 'FAIL: %s does not record an absolute host path for %s\n' "${fingerprint}" "${tool_name}" >&2
    exit 1
  fi
  expected="${recorded}"
fi

if [[ ! -L "${tool}" ]]; then
  printf 'FAIL: %s is not a symlink; the tool would be shipped to a remote executor as a copy\n' "${tool}" >&2
  exit 1
fi
target="$(readlink "${tool}")"
if [[ "${target}" != "${expected}" ]]; then
  printf 'FAIL: %s points at %s, not directly at the host path %s\n' "${tool}" "${target}" "${expected}" >&2
  exit 1
fi
printf 'PASS: %s is a symlink to %s\n' "${tool}" "${expected}"
