#!/bin/bash
# bazel-step-flags.sh -- extra analysis-affecting flags a CI step's tool-install
# shares with its main bazel command.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects. ci_resolve_bazel_step_flags prints a flag list on stdout.
#
# Why: repobin hardcodes --compilation_mode=opt, and bazel_verify_only selects
# --config=ci-strip. If the bootstrap `bazel build //tools/repobin:repobin`
# and `repobin install` do not pass the same --config / -c / --define values as
# the command that follows, Bazel prints "Build option ... has changed,
# discarding analysis cache" and re-analyses the graph (a few seconds per
# step, more on //...). The flags resolved here are appended to CI_BAZEL_FLAGS
# (and therefore REPOBIN_BAZEL_FLAGS) so one server keeps the cache.
#
# A step may set CI_BAZEL_STEP_FLAGS before sourcing ci-env.sh; a non-empty
# value is used as-is. Otherwise:
#   CI_BAZEL_VERIFY_ONLY=1  → --config=ci-strip --compilation_mode=fastbuild
#                             (matches bazel_verify_only)
#   unset                   → --compilation_mode=fastbuild
#                             (matches Bazel default and checkleft's bazel)
# Release steps set CI_BAZEL_STEP_FLAGS=--compilation_mode=opt to match
# `bazel build -c opt`. --config=release and credential --defines stay on the
# Boss.app compile only: they stamp the binary and embed secrets, and must not
# re-key tool-install. That command is the documented exception.

ci_resolve_bazel_step_flags() {
  if [[ -n "${CI_BAZEL_STEP_FLAGS:-}" ]]; then
    printf '%s\n' "$CI_BAZEL_STEP_FLAGS"
    return 0
  fi
  if [[ "${CI_BAZEL_VERIFY_ONLY:-}" == "1" ]]; then
    printf '%s\n' "--config=ci-strip --compilation_mode=fastbuild"
    return 0
  fi
  printf '%s\n' "--compilation_mode=fastbuild"
}
