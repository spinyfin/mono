#!/bin/bash

# Sync boss-v* tags before any Bazel command runs so every agent derives the
# same STABLE_BOSS_BASE_VERSION from tools/boss/installer/workspace-status.sh.
# shellcheck source=fetch-boss-tags.sh
source "$(dirname "${BASH_SOURCE[0]}")/fetch-boss-tags.sh"
echo "--- [ci-env] syncing boss-v* tags from origin"
if ! ci_fetch_boss_tags; then
  echo "^^^ +++"
  echo "[ci-env] error: failed to fetch boss-v* tags from origin (see above). Refusing to run bazel with stale tags that would derive the wrong STABLE_BOSS_BASE_VERSION." >&2
  exit 1
fi

OS_TYPE=$(uname -s | tr '[:upper:]' '[:lower:]')

# Size the disk-cache GC cap to this host's disk. .ci.bazelrc sets
# `build:ci --experimental_disk_cache_gc_max_size=3T` fleet-wide; on any host
# whose cache filesystem is smaller than that, size-based GC never fires and
# the cache fills the disk. A full disk does not take the agent offline -- it
# stays connected and keeps accepting jobs it never runs -- so the cap must fit
# the disk the cache actually lives on. ci_disk_cache_* live in
# disk-cache-gc-cap.sh (tested by //.buildkite/steps/tests). The computed value
# is passed on the command line after --config=ci-<os>, which is where bazel
# lets it override the rc file's value. Nothing here falls back to the 3T
# value: if the cache's filesystem cannot be determined, the step fails.
# shellcheck source=disk-cache-gc-cap.sh
source "$(dirname "${BASH_SOURCE[0]}")/disk-cache-gc-cap.sh"

CI_BAZELRC=".ci.bazelrc"
if ! CI_DISK_CACHE_DIR="$(ci_disk_cache_path "$OS_TYPE" "$CI_BAZELRC")" \
  || ! mkdir -p "$CI_DISK_CACHE_DIR" \
  || ! CI_DISK_CACHE_FS_TOTAL_KB="$(ci_disk_cache_fs_total_kb "$CI_DISK_CACHE_DIR")" \
  || ! CI_DISK_CACHE_GC_MAX_SIZE="$(ci_disk_cache_gc_cap_from_total_kb "$CI_DISK_CACHE_FS_TOTAL_KB")"; then
  echo "^^^ +++"
  echo "[ci-env] error: could not size the Bazel disk-cache GC cap for this host (see above). Refusing to run bazel with .ci.bazelrc's fleet-wide 3T cap: on a smaller disk it never fires and the cache fills the disk." >&2
  exit 1
fi
echo "--- [ci-env] disk cache ${CI_DISK_CACHE_DIR}: filesystem is $((CI_DISK_CACHE_FS_TOTAL_KB / 1048576)) GB, GC cap ${CI_DISK_CACHE_GC_MAX_SIZE} (${CI_DISK_CACHE_GC_PERCENT}%)"

# Decide whether this step's bazel actions execute on remote workers or on
# this agent. The conditions (the step opted in with
# CI_REMOTE_EXECUTION_ALLOWED=1, there are workers for this agent's platform,
# its environment names an endpoint, and the endpoint answers), what executes
# remotely on each platform, and the reasons are in remote-execution.sh, tested
# by //.buildkite/steps/tests. A step that does not qualify runs exactly as
# before.
# shellcheck source=remote-execution.sh
source "$(dirname "${BASH_SOURCE[0]}")/remote-execution.sh"
if ! ci_remote_execution_decide "$OS_TYPE" "$(uname -m)"; then
  echo "^^^ +++"
  echo "[ci-env] error: could not decide where this step's bazel actions execute (see above). Fix or remove CI_BAZEL_REMOTE_EXECUTOR in this agent's environment hook." >&2
  exit 1
fi
echo "--- [ci-env] remote execution ${CI_REMOTE_EXECUTION}: ${CI_REMOTE_EXECUTION_REASON}"
if [[ "$CI_REMOTE_EXECUTION" == "on" ]]; then
  # Buildkite sends SIGTERM when timeout_in_minutes hits, then SIGKILL a few
  # seconds later. Write the executor-naming line first, then a short probe:
  # a 10 s probe against a silent hub uses up the whole cancel grace period
  # and SIGKILL can arrive before the annotation.
  trap '_ci_remote_on_term' TERM
fi
if [[ "$CI_REMOTE_EXECUTION_FELL_BACK" == "1" ]] && command -v buildkite-agent > /dev/null 2>&1; then
  # Building on the agent is correct but slow, and it puts the load back on a
  # host that also serves as a worker. Make it visible on the build page.
  buildkite-agent annotate --style warning --context "ci-remote-execution-${BUILDKITE_JOB_ID:-fallback}" \
    "${BUILDKITE_LABEL:-A step} built on its agent: ${CI_REMOTE_EXECUTION_REASON}." || true
fi

# CI_BAZEL_FLAGS is the single source of truth for the per-command bazel flags
# every CI invocation passes: the OS's CI config, the per-host disk-cache GC
# cap computed above and, when this step executes remotely, the remote config
# and endpoint. The config and the cap must travel together -- any invocation
# that selects --config=ci-<os> without the cap puts the fleet-wide 3T cap back
# in force for the GC pass that follows it. The remote flags add to the OS
# config and replace none of it: the disk cache stays on in front of the remote
# cache (see build:ci-remote in .ci.bazelrc for why). The bazel() wrapper below
# and repobin (via REPOBIN_BAZEL_FLAGS) both read from here.
#
# Analysis-affecting flags this step's main bazel command uses (--config,
# -c / --compilation_mode, --define) are appended so the bootstrap
# `bazel build //tools/repobin:repobin` and `repobin install` keep the same
# analysis cache as the command that follows. A step may set
# CI_BAZEL_STEP_FLAGS before sourcing; otherwise bazel-step-flags.sh fills
# them in from CI_BAZEL_VERIFY_ONLY (see that file). They are appended here
# rather than baked into the assignment below so CI_BAZEL_FLAGS never
# hard-codes --config=ci-strip: release builds share this variable and must
# keep their symbols.
export CI_BAZEL_FLAGS="--config=ci-${OS_TYPE} --experimental_disk_cache_gc_max_size=${CI_DISK_CACHE_GC_MAX_SIZE}${CI_REMOTE_BAZEL_FLAGS:+ ${CI_REMOTE_BAZEL_FLAGS}}"
# shellcheck source=bazel-step-flags.sh
source "$(dirname "${BASH_SOURCE[0]}")/bazel-step-flags.sh"
CI_BAZEL_STEP_FLAGS="$(ci_resolve_bazel_step_flags)"
export CI_BAZEL_STEP_FLAGS
export CI_BAZEL_FLAGS="$CI_BAZEL_FLAGS${CI_BAZEL_STEP_FLAGS:+ ${CI_BAZEL_STEP_FLAGS}}"
export REPOBIN_BAZEL_FLAGS="$CI_BAZEL_FLAGS"
echo "--- [ci-env] bazel step flags: ${CI_BAZEL_STEP_FLAGS}"

# CI_BAZEL_VERIFY_ONLY_FLAGS (set by ci_remote_execution_decide) is for a build
# or test command that is run only for its result: nothing afterwards reads its
# outputs from bazel-bin. When the step executes remotely it is
# --remote_download_minimal, so the agent does not pull gigabytes of binaries
# it will never open (test.log and test.xml still arrive, so the artifact
# upload is unaffected). It is empty otherwise. It is deliberately not part of
# CI_BAZEL_FLAGS: repobin, checkleft and the release steps build a target and
# then run or upload the file, which needs the default of downloading
# top-level outputs. Step scripts reach it through bazel_verify_only, below.

# Bazel daemon idle TTL for every CI pipeline and OS: 30 minutes, so idle
# servers on shared CI hosts (e.g. 16 GB Macs) release their memory. Local
# builds default to 2 hours via `startup --max_idle_secs=7200` in the root
# .bazelrc. CI passes this flag on the command line so it overrides that local
# default; startup options can't be selected via --config. This is the single
# source of truth for CI's value.
BAZEL_MAX_IDLE_SECS=1800

BAZEL_STARTUP_FLAGS="--max_idle_secs=${BAZEL_MAX_IDLE_SECS}"

STARTUP_RC=".ci.${OS_TYPE}.startup.bazelrc"
if [[ -f "$STARTUP_RC" ]]; then
  BAZEL_STARTUP_FLAGS="$BAZEL_STARTUP_FLAGS --bazelrc=$STARTUP_RC"
fi

# CI_BAZEL_STARTUP_FLAGS is the single source of truth for bazel startup
# options in CI. Bazel spins up a brand-new server (and lets the old one
# linger for its full idle TTL) whenever startup options differ from the
# currently running server for the same output_base — so every CI code path
# that shells out to bazel (this script's `bazel()` wrapper below, repobin,
# and checkleft's own `bazel build`/`query` calls for buildifier resolution
# and bazel_aspect invocations) MUST read startup flags from here rather than
# constructing their own, or the workspace ends up running two daemons at
# once (roughly doubling its memory footprint) instead of one.
export CI_BAZEL_STARTUP_FLAGS="$BAZEL_STARTUP_FLAGS"

# A command left on this agent's bazel server by an earlier job would make
# every bazel call below wait on the lock until the step timeout (see
# bazel-server-lock.sh, tested by //.buildkite/steps/tests). Stop that server
# now; fail the step, rather than wait, if that is not possible.
# shellcheck source=bazel-server-lock.sh
source "$(dirname "${BASH_SOURCE[0]}")/bazel-server-lock.sh"
# shellcheck disable=SC2086 # BAZEL_STARTUP_FLAGS is a word list by design.
if ! ci_bazel_release_stale_lock "$STARTUP_RC" "$(pwd -P)" $BAZEL_STARTUP_FLAGS; then
  echo "^^^ +++"
  echo "[ci-env] error: this agent's bazel server is held by an earlier command that could not be stopped (see above)." >&2
  exit 1
fi

# On macOS, detect Xcode version changes and expunge the stale Bazel output
# base. The apple_cc_configure module extension caches Xcode paths in the
# output base; if Xcode is updated without a clean, subsequent builds fail
# with "Xcode version X is not available on the host machine".
if [[ "$OS_TYPE" == "darwin" ]]; then
  CURRENT_XCODE_VERSION=$(xcrun xcodebuild -version 2>/dev/null | tr '\n' ' ' | xargs || echo "unknown")
  XCODE_VERSION_FILE="${HOME}/.cache/bazelcache/.xcode_version"
  if [[ -f "$XCODE_VERSION_FILE" ]]; then
    LAST_XCODE_VERSION=$(cat "$XCODE_VERSION_FILE")
    if [[ "$CURRENT_XCODE_VERSION" != "$LAST_XCODE_VERSION" ]]; then
      echo "--- [ci-env] Xcode changed ('$LAST_XCODE_VERSION' → '$CURRENT_XCODE_VERSION'); expunging stale Bazel output base"
      command bazel $BAZEL_STARTUP_FLAGS clean --expunge
    fi
  fi
  mkdir -p "$(dirname "$XCODE_VERSION_FILE")"
  echo "$CURRENT_XCODE_VERSION" > "$XCODE_VERSION_FILE"
fi

# _ci_note_remote_failure <log of a failed bazel command>
#
# On a step that executes remotely, a bazel command can fail because the
# remote executor failed it: no worker to run an action, an action that lost
# its worker as often as the scheduler allows, a connection that broke. Such a
# failure says nothing about the change, and in the log it is one line among
# thousands. ci_remote_failure_reason (remote-execution.sh) recognises it; this
# puts it on the build page. The step still fails: ci-remote forbids a local
# fallback inside a step, and that does not change here.
_ci_note_remote_failure() {
  local reason
  [[ "$CI_REMOTE_EXECUTION" == "on" ]] || return 0
  reason="$(ci_remote_failure_reason "$1")" || return 0
  echo "^^^ +++"
  echo "[ci-env] this command failed because remote execution failed: ${reason}. See tools/remote-execution/README.md, \"Recovery\"."
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style error --context "ci-remote-execution-failed-${BUILDKITE_JOB_ID:-step}" \
      "${BUILDKITE_LABEL:-A step} failed because remote execution failed, not because an action ran and failed: ${reason}. \`tools/remote-execution/README.md\`, \"Recovery\", says how to tell whether the executor is healthy again; retry the step once it is." || true
  fi
}

_ci_note_watchdog_failure() {
  local reason
  reason="$(cat "$1")"
  echo "^^^ +++"
  echo "[ci-env] this command failed because remote execution failed: ${reason}. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style error --context "ci-remote-execution-failed-${BUILDKITE_JOB_ID:-step}" \
      "${BUILDKITE_LABEL:-A step} failed because remote execution failed, not because an action ran and failed: ${reason}. \`tools/remote-execution/README.md\`, \"What bounds a stuck action\", says how the bound works; retry the step once the executor is healthy." || true
  fi
}

_ci_remote_cleanup_step_watchdog() {
  if [[ -n "${CI_REMOTE_STEP_WATCHDOG_PID:-}" ]]; then
    kill "$CI_REMOTE_STEP_WATCHDOG_PID" 2>/dev/null || true
    wait "$CI_REMOTE_STEP_WATCHDOG_PID" 2>/dev/null || true
  fi
}

if [[ "$CI_REMOTE_EXECUTION" == "on" ]]; then
  # Cover bazel clients this step starts, including ones repobin and
  # checkleft spawn with Command::new("bazel") rather than the bazel()
  # wrapper. Started here so _ci_note_watchdog_failure already exists in
  # the watchdog's subshell. _ci_run_bazel still watches each wrapped
  # command as well.
  CI_REMOTE_WATCHDOG_REASON_FILE="${TMPDIR:-/tmp}/ci-remote-watchdog-${BUILDKITE_JOB_ID:-$$}.reason"
  rm -f "$CI_REMOTE_WATCHDOG_REASON_FILE"
  ci_remote_watchdog_stuck_hook() {
    _ci_note_watchdog_failure "$1"
  }
  ci_remote_watchdog_loop_tree "$$" "$CI_REMOTE_WATCHDOG_REASON_FILE" "${CI_BAZEL_REMOTE_EXECUTOR}" &
  CI_REMOTE_STEP_WATCHDOG_PID=$!
fi

_CI_STEP_EXIT_TRAP="${_CI_STEP_EXIT_TRAP:-}"

_ci_capture_preexisting_exit_trap() {
  local existing
  existing="$(trap -p EXIT)"
  [[ -n "$existing" ]] || return 0
  eval "set -- $existing"
  if [[ "${3:-}" != "_ci_on_exit" ]]; then
    _CI_STEP_EXIT_TRAP="${3:-}"
  fi
}

# Register or clear a step-level EXIT handler without clobbering CI's own
# _ci_on_exit finalizer (which cleans up the watchdog and prints the execlog
# byte summary).
ci_set_exit_trap() {
  _CI_STEP_EXIT_TRAP="$1"
  trap '_ci_on_exit' EXIT
}

ci_clear_exit_trap() {
  _CI_STEP_EXIT_TRAP=""
  trap '_ci_on_exit' EXIT
}

_ci_on_exit() {
  local rc=$?
  set +e
  if [[ -n "${_CI_ON_EXIT_RAN:-}" ]]; then
    return "$rc"
  fi
  _CI_ON_EXIT_RAN=1
  if [[ -n "${_CI_STEP_EXIT_TRAP:-}" ]]; then
    (exit "$rc")
    eval "$_CI_STEP_EXIT_TRAP"
  fi
  _ci_remote_cleanup_step_watchdog
  _ci_execlog_print_summary
  return "$rc"
}

if [[ -z "${_CI_EXIT_TRAP_INSTALLED:-}" ]]; then
  _ci_capture_preexisting_exit_trap
  _CI_EXIT_TRAP_INSTALLED=1
fi
trap '_ci_on_exit' EXIT

_ci_remote_on_term() {
  local msg probed
  msg="$(ci_remote_sigterm_reason_immediate)"
  echo "^^^ +++"
  echo "[ci-env] ${msg}. See tools/remote-execution/README.md, \"What bounds a stuck action\"."
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style error --context "ci-remote-execution-timeout-${BUILDKITE_JOB_ID:-step}" \
      "${BUILDKITE_LABEL:-A step} was killed (SIGTERM) while remote execution was on: ${msg}. \`tools/remote-execution/README.md\`, \"What bounds a stuck action\"." || true
  fi
  probed="$(ci_remote_sigterm_reason)"
  echo "[ci-env] ${probed}."
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style error --context "ci-remote-execution-timeout-${BUILDKITE_JOB_ID:-step}" \
      "${BUILDKITE_LABEL:-A step} was killed (SIGTERM) while remote execution was on: ${probed}. \`tools/remote-execution/README.md\`, \"What bounds a stuck action\"." || true
  fi
  # exit 143 triggers _ci_on_exit, which runs any step EXIT trap, cleans up
  # the watchdog, and prints the execlog byte summary.
  exit 143
}

# Run one bazel command, streaming its output. When the step executes remotely,
# a status watchdog repeats the start-of-step probe while the command runs and
# stops it if the executor goes silent; that is the bound Bazel 9.1 does not
# put on an execution call.
_ci_run_bazel() {
  local tmplog="$1" subcommand="$2"
  shift 2
  # Absolute interpreter captured by _ci_execlog_setup so this hop does not
  # re-enter the PATH bazel shim (the shim covers Command::new("bazel") and
  # other subprocesses that never call this function).
  local bazel_bin="${CI_REAL_BAZEL:-}"
  if [[ -z "$bazel_bin" ]]; then
    bazel_bin="$(command -v bazel)"
  fi
  if [[ "$CI_REMOTE_EXECUTION" != "on" ]]; then
    "$bazel_bin" \
      $BAZEL_STARTUP_FLAGS \
      "$subcommand" \
      $CI_BAZEL_FLAGS \
      "$@" 2>&1 | tee "$tmplog"
    return $?
  fi

  local fifo bazel_pid tee_pid wd_pid rc=0
  fifo=$(mktemp "${tmplog}.fifo.XXXXXX")
  rm -f "$fifo"
  mkfifo "$fifo"
  tee "$tmplog" <"$fifo" &
  tee_pid=$!
  "$bazel_bin" \
    $BAZEL_STARTUP_FLAGS \
    "$subcommand" \
    $CI_BAZEL_FLAGS \
    "$@" >"$fifo" 2>&1 &
  bazel_pid=$!
  rm -f "$tmplog.watchdog"
  ci_remote_watchdog_loop "$bazel_pid" "$tmplog.watchdog" "${CI_BAZEL_REMOTE_EXECUTOR}" &
  wd_pid=$!
  wait "$bazel_pid"
  rc=$?
  kill "$wd_pid" 2>/dev/null || true
  wait "$wd_pid" 2>/dev/null || true
  wait "$tee_pid" 2>/dev/null || true
  rm -f "$fifo"
  return "$rc"
}

# Per-job compact execution logs. Shared with PATH-wrapper subprocesses via
# ci-execlog.sh (unique filenames, sibling-job cleanup, no wipe on re-source).
# shellcheck source=ci-execlog.sh
source "$(dirname "${BASH_SOURCE[0]}")/ci-execlog.sh"
_ci_execlog_setup "$(dirname "${BASH_SOURCE[0]}")/ci-execlog.sh"

# Wrap bazel and pass in ci configuration (CI_BAZEL_FLAGS, above).
# Automatically detects Xcode version mismatch errors (caused by a stale disk
# cache after an Xcode upgrade) and recovers by running `bazel clean --expunge`
# then retrying once.
bazel() {
  local subcommand="$1"
  shift

  local execlog_flag=() flag
  flag="$(_ci_execlog_flag_for "$subcommand" "$@")"
  if [[ -n "$flag" ]]; then
    execlog_flag=("$flag")
  fi

  local tmplog
  tmplog=$(mktemp "${TMPDIR:-/tmp}/ci-bazel-log.XXXXXX")

  if _ci_run_bazel "$tmplog" "$subcommand" ${execlog_flag[@]+"${execlog_flag[@]}"} "$@"; then
    rm -f "$tmplog" "$tmplog.watchdog"
    return 0
  fi

  # Check for Xcode version mismatch (stale disk cache after an Xcode upgrade).
  if grep -qE "xcode-locator.*failed|Xcode version.*is not available" "$tmplog" 2>/dev/null; then
    echo "--- Xcode version mismatch detected in disk cache; running bazel clean --expunge and retrying"
    local bazel_bin="${CI_REAL_BAZEL:-}"
    if [[ -z "$bazel_bin" ]]; then
      bazel_bin="$(command -v bazel)"
    fi
    "$bazel_bin" $BAZEL_STARTUP_FLAGS clean --expunge
    rm -f "$tmplog" "$tmplog.watchdog"
    # Allocate a new path: the first attempt already wrote (or began writing)
    # its log, and each Bazel invocation gets its own file, so the retry must
    # not reuse it.
    execlog_flag=()
    flag="$(_ci_execlog_flag_for "$subcommand" "$@")"
    if [[ -n "$flag" ]]; then
      execlog_flag=("$flag")
    fi
    _ci_run_bazel "$tmplog" "$subcommand" ${execlog_flag[@]+"${execlog_flag[@]}"} "$@"
    return $?
  fi

  if [[ -s "$tmplog.watchdog" ]]; then
    _ci_note_watchdog_failure "$tmplog.watchdog"
    rm -f "$tmplog" "$tmplog.watchdog"
    return 1
  fi

  if [[ -n "${CI_REMOTE_WATCHDOG_REASON_FILE:-}" && -s "$CI_REMOTE_WATCHDOG_REASON_FILE" ]]; then
    _ci_note_watchdog_failure "$CI_REMOTE_WATCHDOG_REASON_FILE"
    rm -f "$tmplog" "$tmplog.watchdog"
    return 1
  fi

  _ci_note_remote_failure "$tmplog"
  rm -f "$tmplog" "$tmplog.watchdog"
  return 1
}

# bazel_verify_only <build|test> <args...>
#
# The bazel() wrapper for a command whose outputs this step never reads from
# disk afterwards: `bazel build //...` and `bazel test //...` run for their
# pass/fail result. Adds CI_BAZEL_VERIFY_ONLY_FLAGS (see above) after the CI
# flags. Do not use it for a build whose output is then executed, uploaded or
# inspected -- on a remotely executing step that file will not be on the agent.
#
# It also relies on --config=ci-strip (see .ci.bazelrc): nothing run through here
# ships or is debugged from its binaries, so Rust executables and test binaries
# are linked without symbol tables, which roughly halves what each one adds to
# the remote cache. Release builds do not go through this function and keep
# their symbols.
#
# The step must set CI_BAZEL_VERIFY_ONLY=1 before sourcing this file so
# CI_BAZEL_FLAGS (and therefore tool-install and this command) already carries
# --config=ci-strip and --compilation_mode=fastbuild. Do not pass --config=ci-strip
# again here: extra_rustc_flag is a repeatable string_list setting, so selecting
# ci-strip twice produces a different configuration than tool-install's single
# selection and discards the analysis cache.
bazel_verify_only() {
  if [[ "${CI_BAZEL_VERIFY_ONLY:-}" != "1" ]]; then
    echo "^^^ +++"
    echo "[ci-env] error: bazel_verify_only requires CI_BAZEL_VERIFY_ONLY=1 before sourcing ci-env.sh so tool-install shares --config=ci-strip and --compilation_mode=fastbuild with this step's main build." >&2
    return 1
  fi
  local subcommand="$1"
  shift
  bazel "$subcommand" $CI_BAZEL_VERIFY_ONLY_FLAGS "$@"
}

# Shared by the release step scripts (checkleft-release.sh, release-release.sh)
# so they don't each carry byte-identical copies. Each script sets
# RELEASE_LOG_PREFIX to its own name before sourcing this file.
die() { echo "error: $*" >&2; exit 1; }

# Sets the RELEASE_TAG global rather than printing to stdout via a command
# substitution: `die` calls `exit 1`, and a `$(...)` substitution runs in a
# subshell, so a die inside one only exits that subshell — the caller's `if
# ! tag="$(release_tag)"` would see a merely-nonzero assignment and take the
# same branch as the documented "no tag" skip, turning a hard failure into a
# silent no-op. Setting a global keeps die's exit reaching the caller's shell.
release_tag() {
  if ! RELEASE_TAG="$(bin/release tag)"; then
    die "bin/release tag failed"
  fi
  if [[ -z "${RELEASE_TAG}" ]]; then
    echo "[${RELEASE_LOG_PREFIX}] no tag from prepare — nothing to build" >&2
    return 1
  fi
}

binary_path() {
  local target="$1" path
  # The bazel() wrapper above does `2>&1 | tee`, so the `2>/dev/null` here does
  # NOT suppress bazel's INFO/Loading/Analyzing lines — on a cold analysis
  # cache those lines can arrive on stdout before the queried path, so the
  # output must be filtered to bazel-out/ lines before taking the first one.
  path="$(bazel cquery -c opt --output=files "${target}" 2>/dev/null | grep '^bazel-out/' | head -1 || true)"
  [[ -n "${path}" && -f "${path}" ]] || die "could not locate Bazel output for ${target}"
  printf '%s\n' "${path}"
}

# Local cube workspaces get the same bin/ layout from .cube/setup.yaml via
# tools/repobin/shim/install-workspace-shims.sh (symlinks that build repobin
# on first use, since a lease cannot afford a bazel build). Keep the two in
# step: bin/checkleft must mean the same binary there as it does here.
#
# Both invocations go through CI_BAZEL_FLAGS / REPOBIN_BAZEL_FLAGS (including
# CI_BAZEL_STEP_FLAGS) so they share the analysis cache with the step's main
# build. repobin's hardcoded --compilation_mode=opt is overridden by the
# trailing --compilation_mode from those flags.
echo "+++ installing repobin tools into bin/"
bazel build //tools/repobin:repobin
./bazel-bin/tools/repobin/repobin install --bin-dir bin/ --no-defaults

