#!/bin/bash
# bazel-server-lock.sh -- release a Bazel server command left behind by an
# earlier job on the same agent, instead of waiting on it until the step times
# out.
#
# Sourced by ci-env.sh (and by its test); defines functions only, no side
# effects.
#
# Why: a Buildkite agent runs one job at a time out of one build directory, and
# Bazel keys its server (and the lock that serialises commands on it) on that
# directory. When a job leaves a command running on the server -- for example
# checkleft kills its `bazel` client on a wall-clock limit while the server is
# still inside the command -- the next job's first bazel call prints
# "Another command (pid=N) is running. Waiting for it to complete on the
# server" and, because --block_for_lock defaults to true, waits for as long as
# the old command lives. Observed on one agent: four consecutive
# bazel-build-test jobs each sat in that wait until the 60 minute step timeout
# and were killed with no test output.
#
# Nothing else legitimately holds the lock when a step starts: the agent runs
# one job at a time, so a command still running on this build directory's
# server at that point belongs to a job that has already ended. The recovery is
# therefore to stop that server (SIGTERM, then SIGKILL) and let the step start
# a new one. That discards only the server's in-memory state: the disk cache,
# the repository cache and the remote cache are untouched.
#
# Functions that report a problem print a `[ci-env]` line on stderr.

# How long to wait for a TERMed server to exit before SIGKILL.
CI_BAZEL_SERVER_TERM_GRACE_SECS="${CI_BAZEL_SERVER_TERM_GRACE_SECS:-20}"

# ci_bazel_output_user_root <startup_bazelrc>
#
# Prints the `startup --output_user_root=<path>` value in <startup_bazelrc>.
# Returns non-zero (printing nothing) when the file or the line is missing, in
# which case the caller cannot locate the server's pid file and skips recovery.
ci_bazel_output_user_root() {
  local rcfile="$1" line root=""
  [[ -r "$rcfile" ]] || return 1
  while IFS= read -r line || [[ -n "$line" ]]; do
    case "$line" in
      "startup --output_user_root="*) root="${line#*--output_user_root=}" ;;
    esac
  done < "$rcfile"
  [[ "$root" == /* ]] || return 1
  printf '%s\n' "$root"
}

# ci_bazel_server_pid <output_user_root> <workspace>
#
# Prints the pid and output base on separate lines for the server belonging to
# <workspace>. Each output base under <output_user_root> holds a
# DO_NOT_BUILD_HERE file containing its workspace path and, while a server owns
# it, server/server.pid.txt. Returns non-zero when no such server is recorded.
ci_bazel_server_pid() {
  local root="$1" workspace="$2" base recorded pid
  for base in "$root"/*/; do
    [[ -r "${base}DO_NOT_BUILD_HERE" && -r "${base}server/server.pid.txt" ]] || continue
    recorded="$(cat "${base}DO_NOT_BUILD_HERE")"
    [[ "$recorded" == "$workspace" ]] || continue
    pid="$(cat "${base}server/server.pid.txt")"
    [[ "$pid" =~ ^[0-9]+$ ]] || continue
    printf '%s\n%s\n' "$pid" "${base%/}"
    return 0
  done
  return 1
}

# _ci_bazel_lock_held <startup flags...>
#
# Succeeds when another command is running on this workspace's Bazel server.
# Probe with `bazel <startup flags> --noblock_for_lock info server_pid`.
# --noblock_for_lock is a startup option, so it precedes the command. Bazel
# exits 9 ("Another command is running. Exiting immediately.") instead of
# waiting; any other result is treated as "not held". Overridden by the test.
_ci_bazel_lock_held() {
  local rc=0
  command bazel "$@" --noblock_for_lock info server_pid > /dev/null 2>&1 || rc=$?
  [[ "$rc" -eq 9 ]]
}

# _ci_bazel_pid_is_bazel_server <pid> <output_base> <workspace>
#
# Require the server marker, server jar, and exact base and workspace startup
# arguments. Read argv as in tools/boss/worker-processes/src/processes.py;
# unlike ps output this preserves argument boundaries, including spaces.
_ci_bazel_pid_is_bazel_server() {
  python3 -c '
import ctypes, os, pathlib, struct, sys
pid, base, workspace = sys.argv[1:]
try:
    if sys.platform == "darwin":
        libc = ctypes.CDLL(None, use_errno=True)
        mib = (ctypes.c_int * 3)(1, 49, int(pid))  # CTL_KERN, KERN_PROCARGS2
        size = ctypes.c_size_t(1024 * 1024)
        buffer = ctypes.create_string_buffer(size.value)
        if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0):
            sys.exit(1)
        argc = struct.unpack_from("i", buffer)[0]
        args = buffer.raw[4:size.value].split(b"\0", 1)[1].lstrip(b"\0").split(b"\0")[:argc]
    else:
        args = pathlib.Path("/proc", pid, "cmdline").read_bytes().rstrip(b"\0").split(b"\0")
    args = [os.fsdecode(arg) for arg in args]
    server = bool(args) and args[0].startswith("bazel(") and any(
        flag == "-jar" and pathlib.Path(value).name == "A-server.jar"
        for flag, value in zip(args, args[1:]))
    sys.exit(0 if server and "--output_base=" + base in args
             and "--workspace_directory=" + workspace in args else 1)
except (OSError, ValueError, IndexError, struct.error):
    sys.exit(1)
' "$1" "$2" "$3"
}

# ci_bazel_release_stale_lock <startup_bazelrc> <workspace> <startup flags...>
#
# If a previous job's command still holds this workspace's Bazel server, stop
# that server. Returns 0 when the lock is free (or was freed) and non-zero,
# after saying why on stderr, when it is held and could not be released. The
# step must then fail rather than wait on the lock.
ci_bazel_release_stale_lock() {
  local startup_rc="$1" workspace="$2"
  shift 2

  _ci_bazel_lock_held "$@" || return 0

  echo "^^^ +++"
  echo "[ci-env] another bazel command already holds this agent's bazel server at step start; it belongs to an earlier job (one job runs at a time per agent). Stopping that server so this step is not stuck waiting on it."

  local root server pid base waited=0
  if ! root="$(ci_bazel_output_user_root "$startup_rc")"; then
    echo "[ci-env] error: cannot find --output_user_root in ${startup_rc}, so the stuck bazel server cannot be located." >&2
    return 1
  fi
  if ! server="$(ci_bazel_server_pid "$root" "$workspace")"; then
    echo "[ci-env] error: no bazel server pid recorded under ${root} for ${workspace}." >&2
    return 1
  fi
  pid="${server%%$'\n'*}"
  base="${server#*$'\n'}"
  if ! _ci_bazel_pid_is_bazel_server "$pid" "$base" "$workspace"; then
    echo "[ci-env] error: recorded bazel server pid ${pid} is not the expected bazel server; refusing to signal it." >&2
    return 1
  fi

  echo "[ci-env] sending SIGTERM to bazel server pid ${pid}"
  kill -TERM "$pid" 2> /dev/null || true
  while kill -0 "$pid" 2> /dev/null && [[ "$waited" -lt "$CI_BAZEL_SERVER_TERM_GRACE_SECS" ]]; do
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "$pid" 2> /dev/null; then
    if ! _ci_bazel_pid_is_bazel_server "$pid" "$base" "$workspace"; then
      echo "[ci-env] error: pid ${pid} changed identity after SIGTERM; refusing SIGKILL." >&2
      return 1
    fi
    echo "[ci-env] bazel server pid ${pid} ignored SIGTERM for ${waited}s; sending SIGKILL"
    kill -KILL "$pid" 2> /dev/null || true
    sleep 1
  fi

  if _ci_bazel_lock_held "$@"; then
    echo "[ci-env] error: bazel server is still locked after stopping pid ${pid}." >&2
    return 1
  fi
  if command -v buildkite-agent > /dev/null 2>&1; then
    buildkite-agent annotate --style warning --context "ci-bazel-stale-lock-${BUILDKITE_JOB_ID:-step}" \
      "${BUILDKITE_LABEL:-A step} found this agent's bazel server held by a command from an earlier job and stopped it (pid ${pid}). Caches were not touched." || true
  fi
  echo "[ci-env] stale bazel server stopped; continuing"
}
