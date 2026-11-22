#!/bin/bash
# ci-execlog.sh -- per-invocation Bazel compact execution-log paths for CI.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects until _ci_execlog_setup is called. Allocates unique
# `--execution_log_compact_file` paths under bazel-execlogs/<job-id>/ so every
# Bazel client this step starts — the bazel() shell wrapper, and subprocesses
# that spawn `bazel` via PATH (repobin, checkleft, the repobin shim) — writes
# its own sequence-numbered `.bin.zst` without colliding.
#
# The compact-log flag cannot live in CI_BAZEL_FLAGS / REPOBIN_BAZEL_FLAGS:
# those are a static string, and two invocations sharing one path overwrite
# each other. Unique paths are allocated at launch instead.

# Return 0 if argv already names an execution log (any of the three mutually
# exclusive flags). Used so a caller that opted into --execution_log_json_file
# (verify-boss-tag-cache.sh) is not also given a compact log.
_ci_execlog_has_flag() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      --execution_log_compact_file|--execution_log_compact_file=*|\
      --execution_log_json_file|--execution_log_json_file=*|\
      --execution_log_binary_file|--execution_log_binary_file=*)
        return 0
        ;;
    esac
  done
  return 1
}

# First positional argument, skipping Bazel startup flags. `bazel --max_idle_secs=1
# build //foo` yields `build`; a bare `bazel build //foo` does too.
_ci_execlog_subcommand() {
  local arg
  for arg in "$@"; do
    case "$arg" in
      -*) continue ;;
      *)
        printf '%s\n' "$arg"
        return 0
        ;;
    esac
  done
  return 0
}

# Allocate the next sequence-numbered path in this job's directory. noclobber
# on a lock file serializes the shell wrapper and a PATH-wrapper child. A
# directory lock is not used: macOS test sandboxes deny rmdir.
_ci_execlog_next_path() {
  local subcommand="$1"
  local lock seq
  command -p mkdir -p "${CI_EXECLOG_DIR:?}"
  lock="${CI_EXECLOG_DIR}/.seq.lock"
  while ! (set -C; printf '%s\n' "$$" > "$lock") 2>/dev/null; do
    command -p sleep 0.05
  done
  seq=0
  if [[ -f "${CI_EXECLOG_DIR}/.seq" ]]; then
    seq="$(command -p cat "${CI_EXECLOG_DIR}/.seq")"
  fi
  seq=$((seq + 1))
  printf '%s\n' "$seq" > "${CI_EXECLOG_DIR}/.seq"
  command -p rm -f "$lock"
  printf '%s/%02d-%s.bin.zst' "$CI_EXECLOG_DIR" "$seq" "$subcommand"
}

# Print `--execution_log_compact_file=<path>` for build/test/run/coverage when
# argv does not already name an execution log; otherwise print nothing.
_ci_execlog_flag_for() {
  local subcommand="$1"
  shift
  case "$subcommand" in
    build|test|run|coverage) ;;
    *) return 0 ;;
  esac
  if _ci_execlog_has_flag "$@"; then
    return 0
  fi
  printf -- '--execution_log_compact_file=%s\n' "$(_ci_execlog_next_path "$subcommand")"
}

# PATH wrapper entry: inject a compact-log flag after the subcommand when
# needed, then exec the real Bazel captured at setup. Startup flags, CI config
# flags, and the caller's own arguments pass through unchanged.
_ci_execlog_wrap_main() {
  local subcommand flag arg
  subcommand="$(_ci_execlog_subcommand "$@")"
  flag="$(_ci_execlog_flag_for "$subcommand" "$@")"
  if [[ -z "$flag" ]]; then
    exec "${CI_REAL_BAZEL:?}" "$@"
  fi
  # Rebuild argv with the compact-log flag immediately after the subcommand so
  # startup options stay startup options. `CI_EXECLOG_WRAP_ARGV` is a throwaway
  # global: this function execs and does not return.
  CI_EXECLOG_WRAP_ARGV=()
  for arg in "$@"; do
    CI_EXECLOG_WRAP_ARGV+=("$arg")
    if [[ "$arg" == "$subcommand" && -n "$flag" ]]; then
      CI_EXECLOG_WRAP_ARGV+=("$flag")
      flag=""
    fi
  done
  exec "${CI_REAL_BAZEL:?}" "${CI_EXECLOG_WRAP_ARGV[@]}"
}

# Create this job's log directory, drop leftover directories from earlier jobs
# on this agent, and prepend a `bazel` shim so Command::new("bazel") / PATH
# lookups from repobin and checkleft go through _ci_execlog_wrap_main.
#
# Idempotent in one job: a second source of ci-env.sh must not delete logs the
# first source already wrote, and must not restart the sequence at 1. Sibling
# job directories under bazel-execlogs/ are removed only on the first setup.
_ci_execlog_setup() {
  local lib="$1"
  local wrap_dir wrap
  if [[ -n "${CI_EXECLOG_SETUP_DONE:-}" ]]; then
    return 0
  fi

  CI_EXECLOG_ROOT="$(pwd -P)/bazel-execlogs"
  CI_EXECLOG_DIR="${CI_EXECLOG_ROOT}/${BUILDKITE_JOB_ID:-local-$$}"
  export CI_EXECLOG_ROOT CI_EXECLOG_DIR

  command -p mkdir -p "$CI_EXECLOG_ROOT"
  for d in "$CI_EXECLOG_ROOT"/*; do
    [[ -e "$d" ]] || continue
    [[ "$(basename "$d")" == "$(basename "$CI_EXECLOG_DIR")" ]] && continue
    command -p rm -rf "$d"
  done
  command -p mkdir -p "$CI_EXECLOG_DIR"

  if [[ -z "${CI_REAL_BAZEL:-}" ]]; then
    # type -P looks at PATH only, so a later bazel() function does not hide the binary.
    CI_REAL_BAZEL="$(type -P bazel || true)"
  fi
  export CI_REAL_BAZEL

  CI_EXECLOG_LIB="$(cd "$(dirname "$lib")" && pwd -P)/$(basename "$lib")"
  export CI_EXECLOG_LIB

  wrap_dir="${CI_EXECLOG_DIR}/.wrap"
  command -p mkdir -p "$wrap_dir"
  wrap="${wrap_dir}/bazel"
  # Tiny trampoline: logic stays in this file so a subprocess can source it.
  # Written with printf — bash 3.2 here-documents go under /tmp, which the
  # test sandbox may deny.
  printf '%s\n' \
    '#!/bin/bash' \
    'set -euo pipefail' \
    'source "${CI_EXECLOG_LIB:?}"' \
    '_ci_execlog_wrap_main "$@"' > "$wrap"
  command -p chmod +x "$wrap"
  export PATH="${wrap_dir}:${PATH}"
  # Drop only the hashed `bazel` entry. `hash -r` would also drop mkdir/rmdir
  # and a hermetic test PATH can then fail to find them.
  hash -d bazel 2>/dev/null || true

  export CI_EXECLOG_SETUP_DONE=1
}
