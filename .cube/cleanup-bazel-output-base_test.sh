#!/usr/bin/env bash
# Tests for cleanup-bazel-output-base.sh, the mono cube cleanup step.
#
# Everything happens in temp directories: fake output bases under a fake
# output_user_root, fake workspaces whose bazel-* symlinks point into them, a
# fake `bazel` first on PATH, and a sleeping process standing in for a Bazel
# server. Nothing here reads or writes the real Bazel cache or any real cube
# workspace.

set -u -o pipefail

script_rel="$1"
script="${TEST_SRCDIR:-.}/${TEST_WORKSPACE:-_main}/$script_rel"
[ -f "$script" ] || { echo "cannot find $script_rel at $script" >&2; exit 2; }

root="$(mktemp -d "${TEST_TMPDIR:-/tmp}/cleanup-test.XXXXXX")"
server_pids=()
cleanup() {
  for pid in "${server_pids[@]:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  chmod -R u+w "$root" 2>/dev/null
  rm -rf "$root"
}
trap cleanup EXIT

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}
assert_eq() { [ "$1" = "$2" ] || fail "$3: expected '$2', got '$1' (last cleanup output: ${output:-none})"; }
assert_exists() { [ -e "$1" ] || fail "$2: $1 should exist"; }
assert_missing() { [ ! -e "$1" ] || fail "$2: $1 should be gone"; }

# A fake `bazel` that records how it was invoked and behaves as the test says.
fakebin="$root/fakebin"
mkdir -p "$fakebin"
# Written with printf, not a here-document: bash 3.2 (macOS) backs here-documents
# with a file in /tmp, which the hermetic test sandbox denies.
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'echo "$PWD|$*" >>"$FAKE_BAZEL_LOG"' \
  'case "${FAKE_BAZEL_MODE:-idle}" in' \
  '  idle) kill "$FAKE_SERVER_PID" 2>/dev/null; exit 0 ;;' \
  '  idle-but-lingers) exit 0 ;;' \
  '  busy) exit 9 ;;' \
  '  fail) exit 2 ;;' \
  'esac' >"$fakebin/bazel"
chmod +x "$fakebin/bazel"

# One fresh scenario per test: an output_user_root holding the shared `cache`
# and `install` trees, plus workspaces and their output bases.
scenario=0
new_scenario() {
  scenario=$((scenario + 1))
  base_root="$root/s$scenario/_bazel_tester"
  workspaces="$root/s$scenario/workspaces"
  mkdir -p "$base_root/cache/repos" "$base_root/install/abc" "$workspaces"
  echo "downloaded" >"$base_root/cache/repos/archive.tar"
  echo "bazel" >"$base_root/install/abc/bazel"
  export FAKE_BAZEL_LOG="$root/s$scenario/bazel.log"
  : >"$FAKE_BAZEL_LOG"
}

# make_workspace NAME -> workspace dir with a checkout file.
make_workspace() {
  mkdir -p "$workspaces/$1"
  echo "source" >"$workspaces/$1/MODULE.bazel"
  echo "$workspaces/$1"
}

# make_base HASH WORKSPACE_DIR -> a populated output base that records
# WORKSPACE_DIR as its owner, with Bazel's read-only directories, and the
# bazel-* symlinks in the workspace pointing at it.
make_base() {
  local base="$base_root/$1" ws="$2" name
  name="$(basename "$ws")"
  mkdir -p "$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/external/some_repo" "$base/server"
  printf '%s' "$ws" >"$base/DO_NOT_BUILD_HERE"
  echo "object" >"$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/external/some_repo/out.o"
  ln -s "$ws/MODULE.bazel" "$base/execroot/_main/MODULE.bazel"
  # Bazel marks its output trees read-only.
  chmod 0555 "$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/external/some_repo" \
    "$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin/external" \
    "$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin"
  ln -sfn "$base/execroot/_main/bazel-out" "$ws/bazel-out"
  ln -sfn "$base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin" "$ws/bazel-bin"
  ln -sfn "$base/execroot/_main" "$ws/bazel-$name"
  echo "$base"
}

# start_server BASE -> a live process recorded as BASE's server. Started in a
# subshell so it is reparented and reaped by init when killed, not left as a
# zombie of this test (a zombie still answers `kill -0`).
start_server() {
  ( sleep 300 </dev/null >/dev/null 2>&1 & echo $! >"$1/server/server.pid.txt" )
  local pid
  pid="$(cat "$1/server/server.pid.txt")"
  server_pids+=("$pid")
  echo "$pid"
}

alive() { kill -0 "$1" 2>/dev/null; }

# run_cleanup WORKSPACE [ENV...] -> sets status and output; cwd is the
# workspace, as when cube runs the step.
run_cleanup() {
  local ws="$1"
  shift
  output="$(cd "$ws" && env PATH="$fakebin:$PATH" CUBE_WORKSPACE="$ws" "$@" bash "$script" 2>&1)"
  status=$?
}

assert_shared_trees_intact() {
  assert_exists "$base_root/cache/repos/archive.tar" "$1: the shared repository cache is never touched"
  assert_exists "$base_root/install/abc/bazel" "$1: bazel's install tree is never touched"
}

# ── no server: the base goes, nothing else does ─────────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
other_ws="$(make_workspace mono-agent-002)"
base="$(make_base aaaa "$ws")"
other_base="$(make_base bbbb "$other_ws")"
run_cleanup "$ws" FAKE_BAZEL_MODE=idle
assert_eq "$status" 0 "no-server cleanup exit"
assert_missing "$base" "no-server cleanup removes the output base, read-only directories included"
assert_exists "$other_base/execroot" "no-server cleanup leaves another workspace's base alone"
assert_exists "$ws/MODULE.bazel" "no-server cleanup leaves the checkout alone"
assert_shared_trees_intact "no-server cleanup"
assert_eq "$(cat "$FAKE_BAZEL_LOG")" "" "no-server cleanup never asks bazel to shut anything down"

# Re-running once it is gone (the bazel-* symlinks now dangle) is a no-op.
run_cleanup "$ws" FAKE_BAZEL_MODE=idle
assert_eq "$status" 0 "re-run after cleanup exit"

# ── stale pid file: no live server, so no shutdown ──────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
( sleep 0.1 </dev/null >/dev/null 2>&1 & echo $! >"$base/server/server.pid.txt" )
sleep 0.5
run_cleanup "$ws"
assert_eq "$status" 0 "stale-pid cleanup exit"
assert_missing "$base" "stale-pid cleanup removes the output base"
assert_eq "$(cat "$FAKE_BAZEL_LOG")" "" "a dead server's pid file does not trigger a shutdown"

# ── idle live server: shut down from the workspace, then removed ────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
pid="$(start_server "$base")"
run_cleanup "$ws" FAKE_BAZEL_MODE=idle FAKE_SERVER_PID="$pid"
assert_eq "$status" 0 "idle-server cleanup exit ($output)"
assert_missing "$base" "idle-server cleanup removes the output base"
assert_eq "$(cat "$FAKE_BAZEL_LOG")" "$ws|--noblock_for_lock shutdown" \
  "the server is shut down from the workspace without waiting on a running command"
alive "$pid" && fail "idle-server cleanup left the server running"
assert_shared_trees_intact "idle-server cleanup"

# ── busy server: reported as busy, nothing touched ──────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
pid="$(start_server "$base")"
run_cleanup "$ws" FAKE_BAZEL_MODE=busy FAKE_SERVER_PID="$pid"
assert_eq "$status" 75 "busy-server cleanup reports busy, not failure"
assert_exists "$base/execroot" "a base with a running Bazel command is never removed"
alive "$pid" || fail "busy-server cleanup killed a server that was in use"
assert_shared_trees_intact "busy-server cleanup"

# ── shutdown failure: a failure, nothing removed ────────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
pid="$(start_server "$base")"
run_cleanup "$ws" FAKE_BAZEL_MODE=fail FAKE_SERVER_PID="$pid"
assert_eq "$status" 75 "failed-shutdown cleanup exit"
assert_exists "$base/execroot" "a base whose server could not be shut down is not removed"

# ── server that will not die: refuse to remove its base ─────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
pid="$(start_server "$base")"
run_cleanup "$ws" FAKE_BAZEL_MODE=idle-but-lingers FAKE_SERVER_PID="$pid" CUBE_CLEANUP_SERVER_EXIT_WAIT_SECONDS=1
assert_eq "$status" 75 "lingering-server cleanup exit"
assert_exists "$base/execroot" "a base whose server is still running is not removed"

# ── a base that belongs to another workspace is refused ─────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
other_ws="$(make_workspace mono-agent-002)"
base="$(make_base aaaa "$other_ws")"
ln -sfn "$base/execroot/_main/bazel-out" "$ws/bazel-out"
run_cleanup "$ws"
assert_eq "$status" 0 "foreign-base cleanup declines without failing"
assert_exists "$base/execroot" "a base recorded as another workspace's is never removed"

# ── symlinks into the shared cache are refused ──────────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
mkdir -p "$base_root/cache/execroot/_main/bazel-out"
ln -sfn "$base_root/cache/execroot/_main/bazel-out" "$ws/bazel-out"
run_cleanup "$ws"
assert_eq "$status" 1 "unmarked tree is withheld because partial removal cannot be ruled out"
assert_exists "$base_root/cache/repos/archive.tar" "a bazel-* link into the shared cache never gets it removed"
assert_exists "$base_root/cache/execroot" "the shared cache directory itself survives"

# ── symlinks disagreeing about where the base is: refuse to guess ───────────
new_scenario
ws="$(make_workspace mono-agent-001)"
other_ws="$(make_workspace mono-agent-002)"
base="$(make_base aaaa "$ws")"
other_base="$(make_base bbbb "$other_ws")"
ln -sfn "$other_base/execroot/_main/bazel-out/darwin_arm64-fastbuild/bin" "$ws/bazel-bin"
run_cleanup "$ws"
assert_eq "$status" 0 "ambiguous-base cleanup declines without failing"
assert_exists "$base/execroot" "ambiguous symlinks remove nothing (first base)"
assert_exists "$other_base/execroot" "ambiguous symlinks remove nothing (second base)"

# ── never built here: nothing to do ─────────────────────────────────────────
new_scenario
ws="$(make_workspace mono-agent-001)"
other_ws="$(make_workspace mono-agent-002)"
other_base="$(make_base bbbb "$other_ws")"
run_cleanup "$ws"
assert_eq "$status" 0 "never-built cleanup exit"
assert_exists "$other_base/execroot" "a workspace that never built removes nobody else's base"
assert_shared_trees_intact "never-built cleanup"

# A legacy interrupted in-place deletion must never be reported as clean.
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
chmod -R u+w "$base"
mkdir -p "$base/external/repo"
rm -rf "$base/execroot" "$base/DO_NOT_BUILD_HERE"
run_cleanup "$ws"
assert_eq "$status" 1 "unverifiable partial base remains withheld"
assert_exists "$base/external/repo" "unverified tree is not removed"

# A crash after atomic retirement leaves a retryable tree even if rm already
# removed execroot and the internal ownership marker.
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
printf '%s\n' "$ws" >"$base.cube-removing-owner"
mv "$base" "$base.cube-removing"
chmod -R u+w "$base.cube-removing"
mkdir -p "$base.cube-removing/external/repo"
rm -rf "$base.cube-removing/execroot" "$base.cube-removing/DO_NOT_BUILD_HERE"
run_cleanup "$ws"
assert_eq "$status" 0 "interrupted retirement retry succeeds"
assert_missing "$base" "partial base never reappears at Bazel's path"
assert_missing "$base.cube-removing" "retry removes remaining external tree"
assert_missing "$base.cube-removing-owner" "retry removes ownership record last"
assert_shared_trees_intact "interrupted retirement"
run_cleanup "$ws"
assert_eq "$status" 0 "completed retirement retry is idempotent"

# A retired tree with a different owner cannot be reclaimed by this workspace.
new_scenario
ws="$(make_workspace mono-agent-001)"
other_ws="$(make_workspace mono-agent-002)"
base="$(make_base aaaa "$ws")"
mv "$base" "$base.cube-removing"
printf '%s\n' "$other_ws" >"$base.cube-removing-owner"
run_cleanup "$ws"
assert_eq "$status" 1 "foreign retired tree stays withheld"
assert_exists "$base.cube-removing/execroot" "foreign retired tree is untouched"

# An empty or partial legacy ownership write can use the still-intact marker.
for partial in '' '/'; do
  new_scenario
  ws="$(make_workspace mono-agent-001)"
  base="$(make_base aaaa "$ws")"
  printf '%s' "$partial" >"$base.cube-removing-owner"
  run_cleanup "$ws"
  assert_eq "$status" 0 "partial initial ownership record recovers"
  assert_missing "$base" "intact marker allows retirement"
done

# Simulate ENOSPC at publication: the base and old record remain untouched.
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
# Inject a failing printf that leaves partial bytes in the temporary record.
printf '%s\n' 'printf() { builtin printf /; return 1; }' >"$root/fail-write.sh"
run_cleanup "$ws" BASH_ENV="$root/fail-write.sh"
assert_eq "$status" 1 "partial temporary record write withholds"
assert_exists "$base/DO_NOT_BUILD_HERE" "partial write leaves marker intact"
assert_missing "$base.cube-removing-owner" "partial bytes never become final record"
printf '%s\n' '#!/bin/sh' 'exit 1' >"$fakebin/mv"
chmod +x "$fakebin/mv"
run_cleanup "$ws"
assert_eq "$status" 1 "failed record publication withholds"
assert_exists "$base/DO_NOT_BUILD_HERE" "failed write leaves base intact"
assert_missing "$base.cube-removing-owner" "failed write never publishes partial record"
rm "$fakebin/mv"
run_cleanup "$ws"
assert_eq "$status" 0 "retry after publication failure succeeds"

# Retry must not rewrite a verified record even when writes would fail.
new_scenario
ws="$(make_workspace mono-agent-001)"
base="$(make_base aaaa "$ws")"
printf '%s\n' "$ws" >"$base.cube-removing-owner"
chmod 0444 "$base.cube-removing-owner"
mv "$base" "$base.cube-removing"
rm "$base.cube-removing/DO_NOT_BUILD_HERE"
printf '%s\n' '#!/bin/sh' 'exit 1' >"$fakebin/rm"
chmod +x "$fakebin/rm"
run_cleanup "$ws"
assert_eq "$status" 1 "interrupted retry withholds"
assert_eq "$(cat "$base.cube-removing-owner")" "$ws" "retry preserves sole ownership evidence"
/bin/rm "$fakebin/rm"
run_cleanup "$ws"
assert_eq "$status" 0 "retry finishes without rewriting read-only record"

# No Python is required to recognize already-cleaned dangling links.
printf '%s\n' '#!/bin/sh' 'exit 1' >"$fakebin/python3"
chmod +x "$fakebin/python3"
run_cleanup "$ws"
assert_eq "$status" 0 "dangling links need no Python"
printf '%s\n' '#!/bin/sh' 'exit 1' >"$fakebin/readlink"
chmod +x "$fakebin/readlink"
run_cleanup "$ws"
assert_eq "$status" 0 "unreadable dangling links are skipped"

if [ "$failures" -ne 0 ]; then
  echo "$failures assertion(s) failed" >&2
  exit 1
fi
echo "all cleanup-bazel-output-base checks passed"
