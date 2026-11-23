#!/usr/bin/env bash
# Inspect the compiler the build resolved, not the text of MODULE.bazel.
#
# MODULE.bazel registers a nightly toolchain for the opt-in Cranelift config
# next to the pinned stable one. The toolchain hub declares that toolchain
# with only the target_settings the root module gave it, and registers the
# hub's toolchains in name order. Without the setting the nightly compiler
# wins every build, release builds included, and nothing reports an error.
set -euo pipefail

expected_channel="${1:?}"
rustc="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}/${2:?}"
sysroot="${rustc%/bin/rustc}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

version="$("${rustc}" --sysroot="${sysroot}" --version)"
echo "resolved compiler: ${version}"

case "${expected_channel}" in
  stable)
    [[ "${version}" != *-nightly* && "${version}" != *-beta* ]] ||
      fail "a build on the stable channel resolved ${version}"
    ;;
  nightly)
    [[ "${version}" == *-nightly* ]] ||
      fail "a build on the nightly channel resolved ${version}"

    # The backend is the component the rules_rust codegen-backends patch
    # downloads into the sysroot. Compiling with it proves that the file is
    # an input of the toolchain and that this compiler can load it.
    work="${TEST_TMPDIR:?}/cranelift"
    mkdir -p "${work}"
    echo 'pub fn answer() -> u32 { 42 }' > "${work}/probe.rs"
    "${rustc}" --sysroot="${sysroot}" -Zcodegen-backend=cranelift \
      --edition=2024 --crate-type=lib --emit=obj \
      -o "${work}/probe.o" "${work}/probe.rs" ||
      fail "the nightly toolchain could not compile with the Cranelift backend"
    [[ -s "${work}/probe.o" ]] || fail "the Cranelift backend produced no object file"
    ;;
  *)
    fail "unknown channel ${expected_channel}"
    ;;
esac
