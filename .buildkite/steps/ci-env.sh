#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Wrap bazel and pass in ci configuration
bazel() {
  local subcommand="$1"
  shift

  local bazelrc_args=()
  [[ -f ".ci.${OS_TYPE}.startup.bazelrc" ]] &&
   	bazelrc_args=(--bazelrc=".ci.${OS_TYPE}.startup.bazelrc")

  command bazel \
   	"${bazelrc_args[@]}" \
    	"$subcommand" \
    	--config="ci-${OS_TYPE}" \
    	"$@"
}
