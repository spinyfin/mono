#!/bin/bash
# Pins that the Cranelift config stays opt-in.
#
# `--config=cranelift` switches the Rust toolchain channel to a nightly
# compiler whose host builds use the Cranelift backend. That is acceptable only
# while someone has to ask for it: a line that selected the config, or that set
# the channel from another config, would move ordinary, `-c opt`, CI or release
# builds onto the nightly compiler, and every one of them would still build.
# //third_party/patches:rust_toolchain_channel_test covers the other half: the
# compiler an ordinary build resolves.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
bazelrc="${workspace}/.bazelrc"
ci_bazelrc="${workspace}/.ci.bazelrc"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Counts the lines of .bazelrc and .ci.bazelrc that are not comments and match
# the extended regular expression $1.
count_active() {
  cat "${bazelrc}" "${ci_bazelrc}" | grep -v -E '^[[:space:]]*#' | grep -c -E -- "$1" || true
}

# The config is one line, and it only switches the channel. The backend flag
# lives on the nightly toolchain in MODULE.bazel, so it cannot reach the
# stable toolchains or the exec configuration.
grep -q -Fx -- 'build:cranelift --@rules_rust//rust/toolchain/channel=nightly' "${bazelrc}" ||
  fail "the cranelift config must switch the Rust toolchain channel to nightly"
[[ "$(count_active '^[a-z]+:cranelift([[:space:]]|$)')" == "1" ]] ||
  fail "the cranelift config must be exactly one line"

# Nothing selects it: not a plain build line, not a platform config, not CI and
# not the release config.
[[ "$(count_active '--config=cranelift([[:space:]]|$)')" == "0" ]] ||
  fail "an rc file selects the cranelift config; it must be passed on the command line"

# No other line changes the channel or names a codegen backend.
[[ "$(count_active 'rust/toolchain/channel')" == "1" ]] ||
  fail "only the cranelift config may set the Rust toolchain channel"
[[ "$(count_active 'codegen-backend')" == "0" ]] ||
  fail "an rc file passes a codegen backend flag; it belongs on the nightly toolchain"
