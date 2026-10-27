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
# workspace. Removing it costs the next build in this workspace a cold start
# (the shared repository/download cache
# next to it is untouched); it does not touch the checkout.
# Cube also reclaims orphan bases; see tools/cube/docs/main.md.
#
# Contract with cube (see tools/cube/src/setup.rs):
#   exit 0   cleaned, nothing to clean, ambiguous links, or an intact foreign
#            base declined (logged to stderr; nothing was touched)
#   exit 75  busy: a Bazel command is running in this workspace; nothing done
#   other    removal failure or an unverifiable partial base; cube keeps the workspace
#            out of the pool and retries
#
# A resolved base without its marker is withheld: legacy partial removal
# cannot be ruled out. After inspecting and repairing it, release the hold
# with `cube workspace gc --clear-failed-cleanup`. Foreign intact bases are
# declined without withholding the workspace.
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

busy() {
  echo "cleanup-bazel-output-base: $*; leaving intact base alone" >&2
  exit "$CLEANUP_BUSY_EXIT"
}

# Nothing has been touched and nothing this step may remove was found.
decline() {
  echo "cleanup-bazel-output-base: $*; nothing cleaned" >&2
  exit 0
}

workspace="${CUBE_WORKSPACE:-$PWD}"
cd "$workspace" || decline "cannot enter workspace $workspace"
physical_workspace="$(pwd -P)"

# 1. Find the output base through the workspace's bazel-* symlinks.
output_base=""
saw_link=0
for link in bazel-*; do
  [ -L "$link" ] || continue
  saw_link=1
  # Resolve live links in the shell. For a dangling link retain its target
  # text so a retry can locate a retired base after execroot was removed.
  target="$(cd -P "$link" 2>/dev/null && pwd -P)" || \
    target="$(readlink "$link")" || continue
  case "$target" in
    */execroot/*) candidate="${target%%/execroot/*}" ;;
    *) continue ;;
  esac
  # Canonicalize the existing parent even when execroot is already gone.
  case "$candidate" in
    /*) ;;
    *) candidate="$physical_workspace/$candidate" ;;
  esac
  parent="$(cd -P "$(dirname "$candidate")" 2>/dev/null && pwd -P)" || continue
  candidate="$parent/$(basename "$candidate")"
  if [ -z "$output_base" ]; then
    output_base="$candidate"
  elif [ "$output_base" != "$candidate" ]; then
    decline "the bazel-* symlinks in $workspace point at different output bases ($output_base and $candidate); refusing to guess"
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
  *) decline "output base '$output_base' is not an absolute path" ;;
esac
marker="$output_base/DO_NOT_BUILD_HERE"
removing="$output_base.cube-removing"
owner="$output_base.cube-removing-owner"
[ ! -L "$owner" ] && [ ! -L "$removing" ] || fail "removal bookkeeping must not be symlinks"

# Keep ownership outside the tree being removed: an interrupted rm may have
# already unlinked every file inside it, including Bazel's ownership marker.
preserve_owner() {
  owner_tmp="$(mktemp "$owner.XXXXXX")" || fail "cannot create ownership record"
  if ! printf '%s\n' "$physical_workspace" >"$owner_tmp" || ! mv -f "$owner_tmp" "$owner"; then
    rm -f "$owner_tmp"
    fail "cannot atomically record ownership before removal"
  fi
}

if [ -e "$removing" ]; then
  retry_marker="$owner"
  [ -f "$retry_marker" ] || retry_marker="$removing/DO_NOT_BUILD_HERE"
  recorded=""
  IFS= read -r recorded <"$retry_marker" || true
  if [ "$recorded" != "$workspace" ] && [ "$recorded" != "$physical_workspace" ]; then
    fail "cannot verify ownership of interrupted removal $removing"
  fi
  # Never rewrite the only surviving evidence on retry.
  [ -f "$owner" ] || preserve_owner
  find "$removing" -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null || true
  rm -rf "$removing" || fail "could not finish removing $removing"
  rm -f "$owner" || fail "could not remove ownership record $owner"
elif [ -e "$owner" ]; then
  # Partial initial records can be recovered using the intact base's marker.
  # Reject a foreign record rather than replacing its ownership evidence.
  recorded=""
  complete_record=0
  [ -f "$owner" ] || fail "ownership record must be a regular file"
  IFS= read -r recorded <"$owner" && complete_record=1
  if [ "$recorded" != "$workspace" ] && [ "$recorded" != "$physical_workspace" ]; then
    [ "$complete_record" -eq 0 ] || fail "foreign ownership record $owner"
    case "$physical_workspace" in
      "$recorded"*) ;;
      *) fail "foreign ownership record $owner" ;;
    esac
  fi
fi
[ -e "$output_base" ] || exit 0
[ -f "$marker" ] || fail "$output_base has no ownership marker and may be partially removed; cannot safely return it to service"
recorded=""
IFS= read -r recorded <"$marker" || true
if [ "$recorded" != "$workspace" ] && [ "$recorded" != "$physical_workspace" ]; then
  decline "$output_base belongs to '$recorded', not to this workspace ($workspace); refusing to remove it"
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
    busy "'bazel shutdown' failed with exit $status in $workspace"
  fi
  waited=0
  while server_alive; do
    if [ "$waited" -ge $((SERVER_EXIT_WAIT_SECONDS * 5)) ]; then
      busy "the Bazel server (pid $server_pid) is still running ${SERVER_EXIT_WAIT_SECONDS}s after 'bazel shutdown'; refusing to remove its output base"
    fi
    sleep 0.2
    waited=$((waited + 1))
  done
fi

# 4. Remove it. Bazel marks directories in its output tree read-only, which
# stops rm -rf partway through, so make them writable first. `find` does not
# follow symlinks, so the links into the checkout inside the execroot are never
# walked through.
preserve_owner
mv "$output_base" "$removing" || fail "cannot atomically retire $output_base"
find "$removing" -type d ! -perm -u+w -exec chmod u+w {} + 2>/dev/null || true
rm -rf "$removing" || fail "could not remove all of $removing"
rm -f "$owner" || fail "cannot remove ownership record $owner"
echo "removed Bazel output base $output_base"
