#!/usr/bin/env bash
# Builds GhosttyKit.xcframework from source for SwiftPM dev builds (swift run Boss).
#
# NOTE — Bazel installer builds do NOT need this script.
# The Bazel build fetches a prebuilt xcframework via http_archive (MODULE.bazel).
# Only run this script when iterating locally with SwiftPM or when cutting a new
# prebuilt release. See tools/boss/docs/runbooks/update-ghostty-prebuilt.md.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE_DIR="$ROOT_DIR/.build-cache"
UPSTREAM_DIR="$CACHE_DIR/ghostty-upstream"
OUTPUT_DIR="$ROOT_DIR/ThirdParty"
FRAMEWORK_DIR="$OUTPUT_DIR/GhosttyKit.xcframework"
TOOLCHAIN_DIR="$CACHE_DIR/toolchains"
# Build a named upstream commit by default. Override only with another
# immutable commit ID, never a moving branch such as `main`.
GHOSTTY_REF="${GHOSTTY_REF:-71c2d68eb40d4e51d30d94e46e7b0c305aa4407f}"
ZIG_VERSION="0.16.0"

if [[ ! "$GHOSTTY_REF" =~ ^[0-9a-f]{40}$ ]]; then
  echo "GHOSTTY_REF must be a full immutable commit ID, not a branch or tag: $GHOSTTY_REF" >&2
  exit 1
fi

case "$(uname -m)" in
  arm64) TARGET_TRIPLE="aarch64-macos.15.0" ;;
  x86_64) TARGET_TRIPLE="x86_64-macos.15.0" ;;
  *)
    echo "unsupported macOS architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

mkdir -p "$CACHE_DIR" "$OUTPUT_DIR" "$TOOLCHAIN_DIR"

ensure_metal_toolchain() {
  if xcrun metal -v >/dev/null 2>&1; then
    return 0
  fi

  cat >&2 <<'EOF'
missing Xcode Metal Toolchain component

Install it with:
  xcodebuild -downloadComponent MetalToolchain
EOF
  exit 1
}

ensure_zig() {
  if command -v brew >/dev/null 2>&1; then
    local brew_prefix
    brew_prefix="$(brew --prefix zig@0.16 2>/dev/null || true)"
    if [[ -n "$brew_prefix" && -x "$brew_prefix/bin/zig" && "$("$brew_prefix/bin/zig" version)" == "$ZIG_VERSION" ]]; then
      echo "$brew_prefix/bin/zig"
      return 0
    fi
  fi

  if command -v zig >/dev/null 2>&1; then
    local system_zig
    system_zig="$(command -v zig)"
    if [[ "$("$system_zig" version)" == "$ZIG_VERSION" ]]; then
      echo "$system_zig"
      return 0
    fi
  fi

  local arch
  case "$(uname -m)" in
    arm64) arch="aarch64" ;;
    x86_64) arch="x86_64" ;;
  esac

  local archive="zig-${arch}-macos-${ZIG_VERSION}.tar.xz"
  local url="https://ziglang.org/download/${ZIG_VERSION}/${archive}"
  local archive_path="$TOOLCHAIN_DIR/$archive"
  local extract_dir="$TOOLCHAIN_DIR/zig-${arch}-macos-${ZIG_VERSION}"
  local zig_bin="$extract_dir/zig"

  if [[ ! -x "$zig_bin" ]]; then
    rm -f "$archive_path"
    curl -L "$url" -o "$archive_path"
    rm -rf "$extract_dir"
    tar -xf "$archive_path" -C "$TOOLCHAIN_DIR"
  fi

  echo "$zig_bin"
}

ensure_metal_toolchain
ZIG_BIN="$(ensure_zig)"
SDKROOT="$(xcrun --sdk macosx --show-sdk-path)"

if [[ ! -d "$UPSTREAM_DIR/.git" ]]; then
  git init --quiet "$UPSTREAM_DIR"
  git -C "$UPSTREAM_DIR" remote add origin https://github.com/ghostty-org/ghostty
fi

# Fetch and check out exactly the requested object. A detached HEAD makes the
# source used for the xcframework explicit even when this cache already exists.
git -C "$UPSTREAM_DIR" fetch --depth 1 origin "$GHOSTTY_REF"
git -C "$UPSTREAM_DIR" reset --hard FETCH_HEAD
CHECKED_OUT_REF="$(git -C "$UPSTREAM_DIR" rev-parse HEAD)"
if [[ "$CHECKED_OUT_REF" != "$GHOSTTY_REF" ]]; then
  echo "Ghostty checkout mismatch: requested $GHOSTTY_REF, got $CHECKED_OUT_REF" >&2
  exit 1
fi

(
  cd "$UPSTREAM_DIR"
  SDKROOT="$SDKROOT" \
  MACOSX_DEPLOYMENT_TARGET=15.0 \
  "$ZIG_BIN" build \
    -Doptimize=ReleaseFast \
    -Dtarget="$TARGET_TRIPLE" \
    -Dapp-runtime=none \
    -Demit-macos-app=false \
    -Demit-xcframework=true \
    -Dxcframework-target=native
)

rm -rf "$FRAMEWORK_DIR"
cp -R "$UPSTREAM_DIR/macos/GhosttyKit.xcframework" "$FRAMEWORK_DIR"

echo
echo "GhosttyKit is ready at:"
echo "  $FRAMEWORK_DIR"
echo
echo "Next:"
echo "  cd $ROOT_DIR"
echo "  swift run Boss"
