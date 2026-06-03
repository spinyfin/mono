#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Wrap bazel and pass in ci configuration
bazel() {
  local subcommand="$1"
  shift

  local startup_rc=".ci.${OS_TYPE}.startup.bazelrc"
  local extra_args=()

  [[ -f "$startup_rc" ]] && extra_args+=(--bazelrc="$startup_rc")

  command bazel \
	"${extra_args[@]}" \
	"$subcommand" \
	--config="ci-${OS_TYPE}" \
	"$@"
}
