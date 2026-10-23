#!/usr/bin/env bash
# Compatibility gate for the boss.spoke.v1 wire schema.
#
#   proto_compat.sh check  <buf> <live-root> <baseline-root> [<merge-base-root>]
#   proto_compat.sh update <buf> <live-root> <baseline-root>
#
# Each root is a directory holding boss/spoke/v1/spoke.proto. buf compiles all
# of them itself, so the check needs no protoc and no network.
#
# `check` (the bazel test) fails when
#   1. the live schema has drifted from the checked-in baseline, i.e. the
#      schema changed without `bazel run //tools/boss/protocol-spoke:update_baseline`
#      (every wire change is deliberate and visible in review), or
#   2. a merge-base baseline is staged (CI does this for pull requests) and the
#      live schema is not backward compatible with it. This is what stops a pull
#      request from making a breaking change and regenerating the baseline to
#      match.
#
# `update` (bazel run) refuses to refresh across a breaking change, then copies
# the live schema over the baseline in the workspace.
set -euo pipefail

mode="${1:?mode}"
buf="${2:?buf}"
live="${3:?live root}"
baseline="${4:?baseline root}"
merge_base="${5:-}"

export BUF_CACHE_DIR="${TEST_TMPDIR:-${TMPDIR:-/tmp}}/buf-cache"
scratch="$(mktemp -d "${TEST_TMPDIR:-${TMPDIR:-/tmp}}/proto-compat.XXXXXX")"
trap 'rm -rf "${scratch}"' EXIT

# FILE-level compatibility (wire, JSON and generated-source) plus the rule that
# a removed field or enum value must reserve both its number and its name.
breaking_config='{"version":"v2","breaking":{"use":["FILE","FIELD_NO_DELETE_UNLESS_NUMBER_RESERVED","FIELD_NO_DELETE_UNLESS_NAME_RESERVED","ENUM_VALUE_NO_DELETE_UNLESS_NUMBER_RESERVED","ENUM_VALUE_NO_DELETE_UNLESS_NAME_RESERVED"],"except":["FIELD_NO_DELETE","ENUM_VALUE_NO_DELETE"]}}'

# buf rejects a directory input once --config is given (it then wants the
# directory to be a declared module), so every root is compiled to an image
# first and the comparisons run on images. Source info (comments) is dropped,
# which makes the image a pure function of the schema.
image() { # <root> <name> -> prints the image path
  "${buf}" build "$1" --exclude-source-info -o "${scratch}/$2.binpb"
  printf '%s' "${scratch}/$2.binpb"
}

# The test sandbox guarantees only a handful of host tools (bash, perl, sed,
# mktemp, rm, ...); there is no cmp or diff, and shasum is not guaranteed. perl
# compares the two images byte for byte using nothing but builtins.
same_bytes() {
  perl -e 'open(my $a, "<:raw", $ARGV[0]) or exit 2; open(my $b, "<:raw", $ARGV[1]) or exit 2; local $/; exit(<$a> eq <$b> ? 0 : 1)' "$1" "$2"
}

backward_compatible() { # <new-image> <old-image>
  "${buf}" breaking "$1" --against "$2" --config "${breaking_config}"
}

case "${mode}" in
check)
  status=0

  live_image="$(image "${live}" live)"
  baseline_image="$(image "${baseline}" baseline)"
  if ! same_bytes "${live_image}" "${baseline_image}"; then
    echo "FAIL: the live boss.spoke.v1 schema differs from the checked-in baseline." >&2
    if ! backward_compatible "${live_image}" "${baseline_image}" >&2; then
      echo "The change above is BREAKING. Do not regenerate the baseline: add the new shape in a new package (boss.spoke.v2) or keep the old field." >&2
    else
      echo "The change is backward compatible. Record it with: bazel run //tools/boss/protocol-spoke:update_baseline" >&2
    fi
    status=1
  fi

  if [[ -n "${merge_base}" && -d "${merge_base}" ]]; then
    echo "checking the live schema against the baseline at the pull request merge base"
    if ! backward_compatible "${live_image}" "$(image "${merge_base}" merge_base)" >&2; then
      echo "FAIL: the live schema is not backward compatible with the contract already on the merge base." >&2
      status=1
    fi
  else
    echo "no merge-base baseline staged (CI stages one for pull requests); checked the checked-in baseline only"
  fi
  exit "${status}"
  ;;

update)
  workspace="${BUILD_WORKSPACE_DIRECTORY:?run with bazel run so the workspace is known}"
  target="${workspace}/tools/boss/protocol-spoke/compat/baseline/boss/spoke/v1/spoke.proto"
  if ! backward_compatible "$(image "${live}" live)" "$(image "${baseline}" baseline)"; then
    echo "refusing to update the baseline: the live schema is not backward compatible with it." >&2
    exit 1
  fi
  cp "${live}/boss/spoke/v1/spoke.proto" "${target}"
  echo "updated ${target}"
  ;;

*)
  echo "unknown mode: ${mode}" >&2
  exit 2
  ;;
esac
