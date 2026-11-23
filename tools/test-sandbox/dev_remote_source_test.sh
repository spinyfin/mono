#!/bin/bash
# Pins the .bazelrc lines that let a developer's Mac build and test on the
# remote workers (README.md, "Remote execution from a developer's Mac").
#
# The mechanism depends on which config a few lines belong to and on their
# order, and none of it is exercised by a build that has no endpoint: an edit
# that looks equivalent can switch it off for every developer, or let the
# local fallback reach Linux, and every test still passes.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
bazelrc="${workspace}/.bazelrc"
ci_bazelrc="${workspace}/.ci.bazelrc"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

# Prints the line number of the one line of .bazelrc that equals $1.
line_number() {
  local matches
  # `|| true`: a line that is missing has to reach the message below, not end
  # the script at the failed grep.
  matches="$(grep -n -Fx -- "$1" "${bazelrc}" | cut -d: -f1 || true)"
  [[ "${matches}" =~ ^[0-9]+$ ]] || fail "expected exactly one line reading: $1"
  echo "${matches}"
}

# Counts the lines of .bazelrc and .ci.bazelrc that are not comments and match
# the extended regular expression $1.
count_active() {
  cat "${bazelrc}" "${ci_bazelrc}" | grep -v -E '^[[:space:]]*#' | grep -c -E -- "$1" || true
}

# The macOS test strategy is a build: line that comes before the line that
# selects the endpoint config. Bazel applies every build: line of a config
# before any test: line of it, so a test:macos strategy would be applied after
# dev-remote's and replace it: tests would quietly run on the developer's Mac.
strategy_line="$(line_number 'build:macos --strategy=TestRunner=local')"
select_line="$(line_number 'build:macos --config=dev-remote-endpoint')"
((strategy_line < select_line)) || fail "the macOS test strategy must come before the dev-remote-endpoint selection"
[[ "$(count_active '^[a-z]+:macos .*--strategy=TestRunner' )" == "1" ]] ||
  fail "there must be exactly one TestRunner strategy line on the macos config"
[[ "$(count_active '^test:macos .*--strategy')" == "0" ]] ||
  fail "a test:macos strategy line overrides dev-remote"

# Without a ~/.bazelrc the endpoint config is two empty defaults and nothing
# else, so a machine that has not opted in builds exactly as before.
line_number 'build:dev-remote-endpoint --remote_executor=' > /dev/null
line_number 'build:dev-remote-endpoint --remote_cache=' > /dev/null
[[ "$(count_active '^[a-z]+:dev-remote-endpoint([[:space:]]|$)')" == "2" ]] ||
  fail "dev-remote-endpoint must have only its two empty defaults in the repo"

# The endpoint itself is machine configuration.
[[ "$(count_active 'grpcs?://')" == "0" ]] || fail "a remote endpoint is written in an rc file"

# dev-remote names `local` as a second test strategy. That is the wrapper's
# Seatbelt profile on macOS and no sandbox at all on Linux, so the config must
# be reachable only through build:macos: the endpoint config is selected from
# that one line, and nothing in the repo selects dev-remote itself.
[[ "$(count_active '--config=dev-remote-endpoint([[:space:]]|$)')" == "1" ]] ||
  fail "dev-remote-endpoint must be selected from build:macos only"
[[ "$(count_active '--config=dev-remote([[:space:]]|$)')" == "0" ]] ||
  fail "dev-remote must be selected from ~/.bazelrc only"

# What an opted-in machine gets.
#
# A disk cache of its own in front of the remote cache, not the directory local
# builds write to: an output that only a disk cache holds is lost to every
# remote action that needs it, and with the shared directory a full test run on
# a machine with a well-used disk cache does not finish. The default disk cache
# has to be a `common` line above the platform-config line, or it is applied
# after dev-remote and puts the shared directory back.
line_number 'build:dev-remote --disk_cache=~/.cache/bazelcache-remote' > /dev/null
[[ "$(count_active '^[a-z]+:dev-remote .*--disk_cache')" == "1" ]] ||
  fail "dev-remote must name exactly one disk cache"
disk_cache_line="$(line_number 'common --disk_cache=~/.cache/bazelcache')"
platform_line="$(line_number 'common --enable_platform_specific_config')"
((disk_cache_line < platform_line)) || fail "the default disk cache must come before the platform-config line"
[[ "$(count_active '^(build|test|run) .*--disk_cache')" == "0" ]] ||
  fail "a plain build/test/run --disk_cache line is applied after dev-remote and overrides it"
# Nothing else may write to dev-remote's directory: a local build into it puts
# back the entries it exists to keep out.
[[ "$(count_active 'bazelcache-remote')" == "1" ]] ||
  fail "only dev-remote may use the remote-mode disk cache directory"
line_number 'build:dev-remote --grpc_keepalive_time=30s' > /dev/null
line_number 'build:dev-remote --grpc_keepalive_timeout=20s' > /dev/null
line_number 'build:dev-remote --workspace_status_command=tools/remote-execution/dev-remote-workspace-status.sh' > /dev/null
# Default workspace-status uses the same precedence as the disk cache: a
# `common` line above the platform-config line. A plain `build` line is
# applied after macos -> dev-remote-endpoint -> dev-remote and would replace
# the watchdog.
workspace_status_line="$(line_number 'common --workspace_status_command=tools/boss/installer/workspace-status.sh')"
((workspace_status_line < platform_line)) || fail "the default workspace-status command must come before the platform-config line"
[[ "$(count_active '^(build|test|run) .*--workspace_status_command')" == "0" ]] ||
  fail "a plain build/test/run --workspace_status_command line is applied after dev-remote and overrides it"

# Effective --workspace_status_command through the automatic macOS config
# (common, then macos -> endpoint -> dev-remote, then plain build lines).
winning_workspace_status() {
  local val="" cfg line
  for cfg in "$@"; do
    case "$cfg" in
      common)
        line="$(grep -E '^common --workspace_status_command=' "${bazelrc}" | tail -n 1 || true)"
        ;;
      build)
        line="$(grep -E '^build --workspace_status_command=' "${bazelrc}" | tail -n 1 || true)"
        ;;
      *)
        line="$(grep -E "^build:${cfg} --workspace_status_command=" "${bazelrc}" | tail -n 1 || true)"
        ;;
    esac
    if [[ -n "$line" ]]; then
      val="${line#*--workspace_status_command=}"
    fi
  done
  printf '%s\n' "$val"
}

effective="$(winning_workspace_status common macos dev-remote-endpoint dev-remote build)"
[[ "$effective" == "tools/remote-execution/dev-remote-workspace-status.sh" ]] ||
  fail "opted-in macOS path must select the developer watchdog, got '${effective}'"
effective="$(winning_workspace_status common macos dev-remote-endpoint build)"
[[ "$effective" == "tools/boss/installer/workspace-status.sh" ]] ||
  fail "macOS path without ~/.bazelrc must keep the default stamp script, got '${effective}'"
effective="$(winning_workspace_status common macos dev-remote-endpoint dev-remote build dev-local)"
[[ "$effective" == "tools/boss/installer/workspace-status.sh" ]] ||
  fail "--config=dev-local must restore the default stamp script, got '${effective}'"
line_number 'build:dev-remote --remote_retries=20' > /dev/null
line_number 'build:dev-remote --remote_retry_max_delay=15s' > /dev/null
line_number 'build:dev-remote --remote_local_fallback' > /dev/null
# Without this a result a developer's Mac produced after a fallback is uploaded
# under the key CI's remote steps use, and a CI step can be satisfied by it.
line_number 'build:dev-remote --noremote_upload_local_results' > /dev/null
line_number 'test:dev-remote --strategy=TestRunner=remote,local' > /dev/null
line_number 'test:dev-remote --test_env=MONO_TEST_REMOTE_EXECUTION=1' > /dev/null
[[ "$(count_active '^[a-z]+:dev-remote .*--strategy')" == "1" ]] ||
  fail "dev-remote must have exactly one strategy line"

# --config=dev-local takes the endpoint away for one command and removes the
# variable, so the result has the key of a plain local run. It must not name a
# strategy: a `local` test strategy on a config anyone may pass by hand would
# switch the sandbox off on Linux.
line_number 'build:dev-local --remote_executor=' > /dev/null
line_number 'build:dev-local --remote_cache=' > /dev/null
# The same disk cache as the default, so a dev-local build shares it.
line_number 'build:dev-local --disk_cache=~/.cache/bazelcache' > /dev/null
line_number 'build:dev-local --workspace_status_command=tools/boss/installer/workspace-status.sh' > /dev/null
line_number 'test:dev-local --test_env==MONO_TEST_REMOTE_EXECUTION' > /dev/null
[[ "$(count_active '^[a-z]+:dev-local .*--strategy')" == "0" ]] ||
  fail "dev-local must not name a strategy"

echo "PASS"
