#!/bin/bash

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

export REPOBIN_BAZEL_FLAGS="--config=ci-${OS_TYPE}"

# Wrap bazel and pass in ci configuration
bazel() {
  local subcommand="$1"
  shift
  command bazel "$subcommand" --config="ci-${OS_TYPE}" "$@"
}
