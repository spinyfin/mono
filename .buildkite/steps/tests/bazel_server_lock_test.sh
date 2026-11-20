#!/bin/bash
#
# Tests for .buildkite/steps/bazel-server-lock.sh, the library ci-env.sh uses
# to stop a bazel server that an earlier job on the same agent left holding the
# command lock.
#
# Only the lock probe is overridden. Identity, lookup and signals run for
# real against throwaway processes with controlled server arguments.
#
# Fixture files are written with printf, not here-documents: bash 3.2 (the
# macOS /bin/bash) backs here-documents with files under /tmp regardless of
# TMPDIR, and the sandbox denies that.

set -euo pipefail

workspace="${TEST_SRCDIR:?}/${TEST_WORKSPACE:?}"
# shellcheck source=../bazel-server-lock.sh
source "${workspace}/.buildkite/steps/bazel-server-lock.sh"

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

tmp="${TEST_TMPDIR:?}"
ws="/fake/agent/builds/mono"
root="${tmp}/output_user_root"
mkdir -p "$root"
printf 'startup --output_user_root=%s\n' "$root" > "${tmp}/startup.bazelrc"
printf '# no startup line here\n' > "${tmp}/empty.bazelrc"

# make_base <name> <workspace> <pid>
make_base() {
  mkdir -p "${root}/$1/server"
  printf '%s' "$2" > "${root}/$1/DO_NOT_BUILD_HERE"
  printf '%s' "$3" > "${root}/$1/server/server.pid.txt"
}

# ---- ci_bazel_output_user_root
got="$(ci_bazel_output_user_root "${tmp}/startup.bazelrc")" || got="<failed>"
[[ "$got" == "$root" ]] || fail "output_user_root: expected ${root}, got ${got}"
if ci_bazel_output_user_root "${tmp}/empty.bazelrc" > /dev/null; then
  fail "output_user_root: a file without the line must fail"
fi
if ci_bazel_output_user_root "${tmp}/missing.bazelrc" > /dev/null; then
  fail "output_user_root: a missing file must fail"
fi

# ---- ci_bazel_server_pid: picks the base whose workspace matches
make_base aaa /some/other/workspace 111
make_base bbb "$ws" 222
got="$(ci_bazel_server_pid "$root" "$ws")" || got="<failed>"
[[ "$got" == "$(printf '222\n%s' "${root}/bbb")" ]] || fail "server_pid: unexpected result ${got}"
if ci_bazel_server_pid "$root" "/no/such/workspace" > /dev/null; then
  fail "server_pid: an unknown workspace must fail"
fi
make_base ccc /third/workspace "not-a-pid"
if ci_bazel_server_pid "$root" "/third/workspace" > /dev/null; then
  fail "server_pid: a non-numeric pid file must fail"
fi

# ---- ci_bazel_release_stale_lock
CI_BAZEL_SERVER_TERM_GRACE_SECS=2
lock_state=held
probe_calls=0
_ci_bazel_lock_held() {
  probe_calls=$((probe_calls + 1))
  [[ "$lock_state" == held ]]
}
spawn_server() {
  bash -c 'exec -a "bazel(mono)" /bin/bash -c "$1" fixture -jar /fixture/A-server.jar "--output_base=$2" "--workspace_directory=$3"' fixture "$1" "$2" "$3" > /dev/null 2>&1 &
  echo $!
}

# A free lock: nothing is read or signalled, and the lock is probed once.
lock_state=free
probe_calls=0
if ! ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "free lock: expected success"
fi
[[ "$probe_calls" -eq 1 ]] || fail "free lock: expected 1 probe, got ${probe_calls}"

# A held lock whose server dies on SIGTERM: the process is gone and the step
# may continue. The probe reports "free" once the server has exited.
# Spawn through a subshell so the process is reparented and reaped on exit;
# a direct child would stay a zombie that `kill -0` still sees.
victim="$(spawn_server 'while :; do sleep 1; done' "${root}/bbb" "$ws")"
sleep 1
make_base bbb "$ws" "$victim"
_ci_bazel_lock_held() { kill -0 "$victim" 2> /dev/null; }
if ! ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "held lock: expected the stale server to be stopped"
fi
sleep 1
if kill -0 "$victim" 2> /dev/null; then
  fail "held lock: server pid ${victim} is still running"
fi

# A server that ignores SIGTERM is killed after the grace period.
stubborn="$(spawn_server 'trap "" TERM; while :; do sleep 1; done' "${root}/bbb" "$ws")"
sleep 1
make_base bbb "$ws" "$stubborn"
_ci_bazel_lock_held() { kill -0 "$stubborn" 2> /dev/null; }
if ! ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "stubborn server: expected SIGKILL to free the lock"
fi
sleep 1
if kill -0 "$stubborn" 2> /dev/null; then
  fail "stubborn server: pid ${stubborn} survived SIGKILL"
fi

# Foreign servers and commands mentioning bazel must fail the real predicate
# and survive the full recovery path.
for kind in workspace base client jar; do
  case "$kind" in
    workspace) bystander="$(spawn_server 'while :; do sleep 1; done' "${root}/bbb" "${ws}-other")" ;;
    base) bystander="$(spawn_server 'while :; do sleep 1; done' "${root}/bbb-other" "$ws")" ;;
    client) bystander="$(bash -c 'while :; do sleep 1; done' bazel --output_base="${root}/bbb" > /dev/null 2>&1 & echo $!)" ;;
    jar) bystander="$(bash -c 'exec -a "bazel(mono)" /bin/bash -c "while :; do sleep 1; done" fixture -jar /other.jar "--output_base=$1" "--workspace_directory=$2"' fixture "${root}/bbb" "$ws" > /dev/null 2>&1 & echo $!)" ;;
  esac
  sleep 1
  make_base bbb "$ws" "$bystander"
  _ci_bazel_lock_held() { return 0; }
  if ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" > /dev/null 2>&1; then
    fail "$kind: expected identity rejection"
  fi
  kill -0 "$bystander" 2> /dev/null || fail "$kind: bystander was signalled"
  kill -KILL "$bystander" 2> /dev/null || true
done

# Simulate PID reuse during the grace period by replacing the server argv
# after TERM. The second identity check must prevent SIGKILL.
reused="$(spawn_server 'trap "exec sleep 300" TERM; while :; do sleep 1; done' "${root}/bbb" "$ws")"
sleep 1
make_base bbb "$ws" "$reused"
_ci_bazel_lock_held() { return 0; }
if ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" > /dev/null 2>&1; then
  fail "changed identity: expected refusal to escalate"
fi
kill -0 "$reused" 2> /dev/null || fail "changed identity: process was killed"
kill -KILL "$reused" 2> /dev/null || true

# A held lock whose recorded pid is not a bazel process is never signalled.
bystander="$(sleep 300 > /dev/null 2>&1 & echo $!)"
make_base bbb "$ws" "$bystander"
_ci_bazel_lock_held() { return 0; }
if ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "$ws" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "unrelated pid: expected failure"
fi
kill -0 "$bystander" 2> /dev/null || fail "unrelated pid: the bystander process was signalled"
kill -KILL "$bystander" 2> /dev/null || true

# A held lock with no recorded server, or no way to find one, fails.
if ci_bazel_release_stale_lock "${tmp}/startup.bazelrc" "/no/such/workspace" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "no recorded server: expected failure"
fi
if ci_bazel_release_stale_lock "${tmp}/empty.bazelrc" "$ws" --max_idle_secs=1 > /dev/null 2>&1; then
  fail "no output_user_root: expected failure"
fi

if [[ "$failures" -ne 0 ]]; then
  echo "${failures} assertion(s) failed" >&2
  exit 1
fi
echo "ok"
