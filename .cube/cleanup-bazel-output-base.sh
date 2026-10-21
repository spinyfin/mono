#!/usr/bin/env bash
# Cube cleanup step for mono workspaces: give back this workspace's Bazel
# output base.
#
# Declared under `cleanup:` in .cube/setup.yaml. cube decides when a free
# workspace may be cleaned (disk pressure) and guarantees nothing leases it
# while this runs; what "clean" means for mono is all in this file, so cube
# itself stays ignorant of Bazel.
#
# Bazel keeps each workspace's build state in a private output base outside the
# checkout (under `output_user_root`), 10-16 GiB for a fully built mono
# workspace and never reclaimed by anything else. Removing it costs the next
# build in this workspace a cold start (the shared repository/download cache
# next to it is untouched); it does not touch the checkout.
#
# Contract with cube (see tools/cube/src/setup.rs):
#   exit 0   cleaned, or nothing to clean
#   exit 75  busy: a Bazel command is running in this workspace; nothing done
#   other    failure; cube keeps the workspace out of the pool and retries
#
# How the output base is found: from this workspace's own `bazel-*` symlinks
# (they point at <output_base>/execroot/<name>/...), never by guessing at the
# hash Bazel names the directory with, and it is only removed once the
# `DO_NOT_BUILD_HERE` file Bazel writes in it names exactly this workspace. That
# is what keeps the shared `cache` directory, Bazel's `install` tree and every
# other workspace's base out of reach: none of them belong to this workspace.
#
# The Bazel server is shut down with `bazel --noblock_for_lock shutdown`, which
# fails with exit 9 instead of waiting when a command holds the output base
# lock, so a server that is idle is stopped and one that is building is left
# alone. Never `bazel clean --expunge`.

set -u -o pipefail

BAZEL_LOCK_HELD_EXIT=9
CLEANUP_BUSY_EXIT=75
# Overridable only so the test does not have to wait out the real bound.
SERVER_EXIT_WAIT_SECONDS="${CUBE_CLEANUP_SERVER_EXIT_WAIT_SECONDS:-30}"

fail() {
  echo "cleanup-bazel-output-base: $*" >&2
  exit 1
}

workspace="${CUBE_WORKSPACE:-$PWD}"
cd "$workspace" || fail "cannot enter workspace $workspace"
physical_workspace="$(pwd -P)"

# 1. Find the output base through the workspace's bazel-* symlinks.
output_base=""
saw_link=0
for link in bazel-*; do
  [ -L "$link" ] || continue
  saw_link=1
  # Resolved in the shell (`cd -P`) rather than with readlink, which not every
  # environment cube's steps run in provides. A link whose target is gone
  # (already cleaned) does not resolve and is skipped.
  target="$(cd -P "$link" 2>/dev/null && pwd -P)" || continue
  case "$target" in
    */execroot/*) candidate="${target%%/execroot/*}" ;;
    *) continue ;;
  esac
  if [ -z "$output_base" ]; then
    output_base="$candidate"
  elif [ "$output_base" != "$candidate" ]; then
    fail "the bazel-* symlinks in $workspace point at different output bases ($output_base and $candidate); refusing to guess"
  fi
done

if [ -z "$output_base" ]; then
  if [ "$saw_link" -eq 0 ]; then
    echo "no bazel-* symlinks in $workspace; Bazel has not built here, nothing to clean"
  else
    echo "the bazel-* symlinks in $workspace do not lead to an output base (already cleaned?); nothing to clean"
  fi
  exit 0
fi

# 2. It must be this workspace's base, by Bazel's own record of whose it is.
case "$output_base" in
  /?*) ;;
  *) fail "output base '$output_base' is not an absolute path" ;;
esac
marker="$output_base/DO_NOT_BUILD_HERE"
[ -f "$marker" ] || fail "$output_base has no DO_NOT_BUILD_HERE; it is not a Bazel output base, refusing to remove it"
recorded=""
IFS= read -r recorded <"$marker" || true
if [ "$recorded" != "$workspace" ] && [ "$recorded" != "$physical_workspace" ]; then
  fail "$output_base belongs to '$recorded', not to this workspace ($workspace); refusing to remove it"
fi

# 3. A live server is shut down if it is idle and means "busy" if it is not.
pid_file="$output_base/server/server.pid.txt"
server_pid=""
if [ -f "$pid_file" ]; then
  IFS= read -r server_pid <"$pid_file" || true
fi
server_alive() {
  case "$server_pid" in
    '' | *[!0-9]*) return 1 ;;
  esac
  kill -0 "$server_pid" 2>/dev/null
}

if server_alive; then
  bazel --noblock_for_lock shutdown
  status=$?
  if [ "$status" -eq "$BAZEL_LOCK_HELD_EXIT" ]; then
    echo "a Bazel command is running in $workspace (server pid $server_pid); leaving its output base alone" >&2
    exit "$CLEANUP_BUSY_EXIT"
  elif [ "$status" -ne 0 ]; then
    fail "'bazel shutdown' failed with exit $status in $workspace"
  fi
  waited=0
  while server_alive; do
    if [ "$waited" -ge $((SERVER_EXIT_WAIT_SECONDS * 5)) ]; then
      fail "the Bazel server (pid $server_pid) is still running ${SERVER_EXIT_WAIT_SECONDS}s after 'bazel shutdown'; refusing to remove its output base"
    fi
    sleep 0.2
    waited=$((waited + 1))
  done
fi

# 4. Remove it. Bazel marks directories in its output tree read-only, which
# stops rm -rf partway through, so make them writable first. `find` does not
# follow symlinks, so the links into the checkout inside the execroot are never
# walked through.
find "$output_base" -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null || true
rm -rf "$output_base"
if [ -e "$output_base" ]; then
  fail "could not remove all of $output_base"
fi
echo "removed Bazel output base $output_base"
