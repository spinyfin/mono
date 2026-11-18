#!/bin/bash
# build-worker.sh -- build the NativeLink binary the macOS workers run.
#
# The workers cannot run an upstream release: on macOS upstream hardlinks
# executables out of the worker's store, dyld then resolves @loader_path
# against the store, and rustc fails to load its own driver library. The fix
# is the patch in patches/ and it is not upstream yet (see ../README.md,
# "Version and patch"). So the worker binary is the pinned upstream release
# with that patch, built from source on a Mac, and this script is the whole
# recipe: there is no fork branch to keep in step.
#
# The hub does not need this. It runs on Linux and stages no executables, so
# it uses the upstream release binary (../README.md has the digest).
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
# Run it on a Mac with the Xcode the workers have. It needs git and bazel
# (bazelisk picks the version the checkout pins) and builds for some tens of
# minutes the first time.
set -euo pipefail

# What every worker runs. Change these together with patches/ and with the
# version the runbook names; ../tests/config_test.sh keeps the three in step.
NATIVELINK_REPOSITORY="https://github.com/TraceMachina/nativelink"
NATIVELINK_VERSION="1.8.6"
NATIVELINK_COMMIT="37f3b206a8e6d55613abae2b18a0665fff24e096"
# The suffix the patch adds to the version the binary reports.
PATCH_MARKER="macos-clonefile"

# The tests that cover what the patch changes. They are macOS-only tests, so
# they run nothing on another platform, which is why this script refuses one.
PATCH_TESTS=(
  //nativelink-util:integration
  //nativelink-worker:integration
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
[[ "$(uname -s)" == "Darwin" ]] || die "the worker binary is built on macOS; the hub uses the upstream release"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
patches=("$script_dir"/patches/*.patch)
[[ -f "${patches[0]}" ]] || die "no patches found in $script_dir/patches"

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
fi

[[ -d "$src/.git" || -f "$src/.git" ]] || die "$src is not a git checkout"
head="$(git -C "$src" rev-parse HEAD)"
[[ "$head" == "$NATIVELINK_COMMIT" ]] ||
  die "$src is at $head, not the pinned ${NATIVELINK_COMMIT}; use a fresh --src"
[[ -z "$(git -C "$src" status --porcelain --untracked-files=all | grep -v '^ M ' || true)" ]] ||
  die "$src has untracked, staged or deleted files; use a fresh --src"
if ! diff <(expected_diff) <(applied_diff) > /dev/null; then
  die "$src does not differ from the pinned commit by exactly the patches in $script_dir/patches; use a fresh --src"
fi
echo "--- [build-worker] $src is v${NATIVELINK_VERSION} plus ${#patches[@]} patch(es)"

if [[ "$prepare_only" == 1 ]]; then
  exit 0
fi

echo "--- [build-worker] building and running the tests that cover the patch"
(
  cd "$src"
  bazel test -c opt "${PATCH_TESTS[@]}" //:nativelink
)

mkdir -p "$out"
install -m 755 "$src/bazel-bin/nativelink" "$out/nativelink"
# The patch gives the binary a version of its own, so `nativelink --version`
# on a worker shows that it is this build and not an upstream release.
reported="$("$out/nativelink" --version)"
[[ "$reported" == "nativelink ${NATIVELINK_VERSION}+${PATCH_MARKER}" ]] ||
  die "the binary reports '${reported}', expected 'nativelink ${NATIVELINK_VERSION}+${PATCH_MARKER}'"
# A release built with Nix once linked a library by its /nix/store path and
# did not start on a Mac without Nix. A source build links the system copy;
# check rather than assume.
if otool -L "$out/nativelink" | grep -q '/nix/store'; then
  die "the binary links a /nix/store library and will not start on a worker"
fi
(cd "$out" && shasum -a 256 nativelink > nativelink.sha256)
echo "--- [build-worker] built $out/nativelink"
echo "${reported}, sha256 $(cut -d' ' -f1 "$out/nativelink.sha256")"
echo "Every worker must run this exact file: compare the digest after copying it."
