#!/bin/bash
# build-worker.sh -- build the NativeLink binary the workers run.
#
# The workers do not run an upstream release. Two fixes they need are not
# upstream yet (see ../README.md, "Version and patches"):
#
#   - On macOS upstream hardlinks executables out of the worker's store, dyld
#     then resolves @loader_path against the store, and rustc fails to load
#     its own driver library.
#   - On every platform a store entry whose file is gone is never fetched
#     again, and every action that needs the blob fails until the worker
#     restarts.
#
# So the worker binary is the pinned upstream release with the patches in
# patches/ that apply to the platform, built from source on a host of that
# platform, and this script is the whole recipe: there is no fork branch to
# keep in step.
#
# The hub does not need this. It stages no inputs, so it uses the upstream
# release binary (../README.md has the digest).
#
#   build-worker.sh --src <dir> --out <dir> [--prepare-only]
#
#   --src           Checkout to build in. Created if missing: the pinned commit
#                   is fetched and the patches applied. If it exists it must be
#                   that commit with exactly those patches applied, and nothing
#                   else; anything different is an error, never rebuilt over.
#                   Keep the directory between builds to keep bazel's outputs.
#   --out           Where to put the result: the `nativelink` binary and
#                   `nativelink.sha256`.
#   --prepare-only  Stop after the checkout is prepared and verified.
#
# Run it on a Mac with the Xcode the workers have, for the macOS workers, and
# on a Linux host with the distribution the workers have, for the Linux ones:
# the Linux binary links the host's libc. It needs git and bazel (bazelisk
# picks the version the checkout pins) and builds for some tens of minutes the
# first time.
set -euo pipefail

# What every worker runs. Change these together with patches/ and with the
# version the runbook names; ../tests/config_test.sh keeps the three in step.
NATIVELINK_REPOSITORY="https://github.com/TraceMachina/nativelink"
NATIVELINK_VERSION="1.8.6"
NATIVELINK_COMMIT="37f3b206a8e6d55613abae2b18a0665fff24e096"
# The patches, in the order they are applied. Each line is the file in
# patches/, the marker it adds to the version the binary reports, and the
# platforms (`uname -s`) whose workers carry it.
PATCHES=(
  "macos-clonefile-executables.patch macos-clonefile Darwin"
  "stale-entry-recovery.patch stale-entry Darwin,Linux"
)

die() {
  echo "build-worker: error: $*" >&2
  exit 1
}

src=""
out=""
prepare_only=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --src) src="${2:-}"; shift 2 ;;
    --out) out="${2:-}"; shift 2 ;;
    --prepare-only) prepare_only=1; shift ;;
    *) die "unknown argument: $1" ;;
  esac
done
[[ -n "$src" ]] || die "--src is required"
[[ "$prepare_only" == 1 || -n "$out" ]] || die "--out is required"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
platform="$(uname -s)"
patches=()
markers=()
for entry in "${PATCHES[@]}"; do
  read -r patch_file patch_marker patch_platforms <<< "$entry"
  [[ -f "$script_dir/patches/$patch_file" ]] || die "$patch_file is not in $script_dir/patches"
  if [[ ",${patch_platforms}," == *",${platform},"* ]]; then
    patches+=("$script_dir/patches/$patch_file")
    markers+=("$patch_marker")
  fi
done
[[ ${#patches[@]} -gt 0 ]] || die "no patch applies to ${platform}; workers are built on macOS and Linux"
# What `nativelink --version` reports for this build: the release, then the
# marker of every patch in it, so a worker's binary says what it carries and
# an upstream release given to a worker by mistake can be told apart.
reported_version="${NATIVELINK_VERSION}+$(IFS=.; echo "${markers[*]}")"

# The tests that cover what the patches change. Neither platform can run all
# of upstream's suites, patch or no patch:
#   - macOS: the store's test file does not pass upstream's own lint there, so
#     the store-level tests of the stale-entry patch run on Linux only. The
#     worker-level test, which stages a tree through both paths the patch
#     changes, runs on both.
#   - Linux: the worker's suite has tests of process control that a bazel
#     sandbox on these hosts does not let pass, so its tests are named one by
#     one and filtered.
case "$platform" in
  Darwin)
    patch_tests=(
      //nativelink-util:integration
      //nativelink-worker:integration
    )
    ;;
  Linux)
    patch_tests=(
      //nativelink-store:integration_tests/filesystem_store_test_test
      //nativelink-worker:integration_tests/running_actions_manager_test_test
      --test_arg=download_to_directory
      --test_arg=variant
      --test_arg=forget_if_missing
    )
    ;;
esac

# Gives the binary its version. A Bazel build otherwise reports 0.0.0. It is a
# patch like the others so that the checkout check below covers it, written
# here because the version depends on which patches the platform takes.
stamp_version() {
  git -C "$1" apply <<EOF
diff --git a/BUILD.bazel b/BUILD.bazel
--- a/BUILD.bazel
+++ b/BUILD.bazel
@@ -44,6 +44,8 @@ rust_binary(
     srcs = [
         "src/bin/nativelink.rs",
     ],
+    # Stamped by build-worker.sh: the release and the patches applied to it.
+    version = "${reported_version}",
     deps = [
         ":nativelink_lib",
         "//nativelink-config",
EOF
}

# The checkout's difference from the pinned commit, as one patch. Comparing
# this with the patches is what proves a reused checkout is what it claims.
applied_diff() {
  git -C "$src" diff --no-color --no-ext-diff "$NATIVELINK_COMMIT" --
}

expected_diff() {
  local scratch
  scratch="$(mktemp -d)"
  git -C "$src" worktree add --quiet --detach "$scratch/tree" "$NATIVELINK_COMMIT"
  local patch
  for patch in "${patches[@]}"; do
    git -C "$scratch/tree" apply "$patch"
  done
  stamp_version "$scratch/tree"
  git -C "$scratch/tree" diff --no-color --no-ext-diff "$NATIVELINK_COMMIT" --
  git -C "$src" worktree remove --force "$scratch/tree"
  rm -rf "$scratch"
}

if [[ ! -e "$src" ]]; then
  echo "--- [build-worker] fetching NativeLink v${NATIVELINK_VERSION} (${NATIVELINK_COMMIT})"
  mkdir -p "$src"
  git -C "$src" init --quiet
  git -C "$src" fetch --quiet --depth 1 "$NATIVELINK_REPOSITORY" "$NATIVELINK_COMMIT"
  git -C "$src" checkout --quiet --detach FETCH_HEAD
  for patch in "${patches[@]}"; do
    echo "--- [build-worker] applying $(basename "$patch")"
    git -C "$src" apply "$patch"
  done
  stamp_version "$src"
fi

[[ -d "$src/.git" || -f "$src/.git" ]] || die "$src is not a git checkout"
head="$(git -C "$src" rev-parse HEAD)"
[[ "$head" == "$NATIVELINK_COMMIT" ]] ||
  die "$src is at $head, not the pinned ${NATIVELINK_COMMIT}; use a fresh --src"
[[ -z "$(git -C "$src" status --porcelain --untracked-files=all | grep -v '^ M ' || true)" ]] ||
  die "$src has untracked, staged or deleted files; use a fresh --src"
if ! diff <(expected_diff) <(applied_diff) > /dev/null; then
  die "$src does not differ from the pinned commit by exactly the ${platform} patches in $script_dir/patches and the version ${reported_version}; use a fresh --src"
fi
echo "--- [build-worker] $src is v${NATIVELINK_VERSION} plus ${#patches[@]} patch(es)"

if [[ "$prepare_only" == 1 ]]; then
  exit 0
fi

echo "--- [build-worker] building and running the tests that cover the patches"
(
  cd "$src"
  bazel test -c opt "${patch_tests[@]}" //:nativelink
)

mkdir -p "$out"
install -m 755 "$src/bazel-bin/nativelink" "$out/nativelink"
reported="$("$out/nativelink" --version)"
[[ "$reported" == "nativelink ${reported_version}" ]] ||
  die "the binary reports '${reported}', expected 'nativelink ${reported_version}'"
if [[ "$platform" == "Darwin" ]]; then
  # A release built with Nix once linked a library by its /nix/store path and
  # did not start on a Mac without Nix. A source build links the system copy;
  # check rather than assume.
  if otool -L "$out/nativelink" | grep -q '/nix/store'; then
    die "the binary links a /nix/store library and will not start on a worker"
  fi
  (cd "$out" && shasum -a 256 nativelink > nativelink.sha256)
else
  (cd "$out" && sha256sum nativelink > nativelink.sha256)
fi
echo "--- [build-worker] built $out/nativelink"
echo "${reported}, sha256 $(cut -d' ' -f1 "$out/nativelink.sha256")"
echo "Every ${platform} worker must run this exact file: compare the digest after copying it."
