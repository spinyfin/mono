#!/bin/bash
#
# Guard: the audited runtime's tools must reach a test action as symlinks to
# the host's own binaries, never as files.
#
# test_runtime_repository once exposed each tool with repository_ctx.symlink.
# Bazel treats such a link as a source file, so a remote executor received a
# copy of the client's binary, and macOS kills a copy of a platform binary at
# launch (see README.md, "Remote execution on macOS workers"). Locally a copy
# and a symlink behave the same, so nothing but a remote run would notice a
# change back.
#
# What does differ locally is the link itself. Bazel stages a declared,
# unresolved symlink verbatim: bin/<tool> points straight at the host path the
# manifest records. Anything else it stages (a source file, a copied output)
# is linked to a path inside Bazel's own trees, or arrives as a plain file.

set -euo pipefail

runtime_root="${TEST_SRCDIR:?}/+test_runtime_repository+test_runtime_tools"
manifest="${runtime_root}/manifest"

if [[ ! -f "${manifest}" ]]; then
  printf 'audited runtime manifest not found: %s\n' "${manifest}" >&2
  exit 1
fi

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

declared=" "
checked=0
while IFS='=' read -r name recorded; do
  if [[ "${name}" == *-tree ]]; then
    continue
  fi
  declared+="${name} "
  checked=$((checked + 1))
  tool="${runtime_root}/bin/${name}"
  if [[ ! -L "${tool}" ]]; then
    fail "bin/${name} is not a symlink; the tool would be shipped to a remote executor as a copy"
    continue
  fi
  target="$(readlink "${tool}")"
  if [[ "${target}" != "${recorded}" ]]; then
    fail "bin/${name} points at ${target}, not directly at the audited host path ${recorded}"
  fi
done <"${manifest}"

if grep -q '^sample=' "${manifest}"; then
  fail "sample must not be in the shared test runtime; timeout capture owns it"
fi

if [[ "${checked}" -eq 0 ]]; then
  fail "the runtime manifest lists no tools, so nothing was checked"
fi

# Nothing may sit in bin/ (and so on every test's PATH) beside the audited
# tools the manifest lists.
for tool in "${runtime_root}"/bin/*; do
  name="${tool##*/}"
  if [[ "${declared}" != *" ${name} "* ]]; then
    fail "bin/${name} is on the test PATH but is not in the runtime manifest"
  fi
done

if [[ "${failures}" -ne 0 ]]; then
  exit 1
fi
printf 'PASS: %s runtime tools are symlinks to their audited host paths\n' "${checked}"
