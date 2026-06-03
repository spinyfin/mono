#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Wrap bazel and pass in ci configuration
bazel() {
  local subcommand="$1"
  shift

  local bazelrc_arg=""
  if [[ -f ".ci.${OS_TYPE}.startup.bazelrc" ]]; then
    bazelrc_arg="--bazelrc=.ci.${OS_TYPE}.startup.bazelrc"
  fi

  command bazel \
    $bazelrc_arg \
    "$subcommand" \
    --config="ci-${OS_TYPE}" \
    "$@"
}
