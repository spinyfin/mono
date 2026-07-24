#!/usr/bin/env bash
# stub-ghosttykit-xcframework.sh — materialize a minimal placeholder
# GhosttyKit.xcframework so any Bazel *analysis* of the macOS app can run.
#
# Why this exists:
#   rules_swift_package_manager's swift_deps module extension runs
#   `swift package describe` against tools/boss/app-macos/Package.swift during
#   EVERY Bazel analysis — `bazel build`, `bazel test`, AND
#   `bazel mod deps --lockfile_mode=update`. Package.swift declares
#   .binaryTarget(path: "ThirdParty/GhosttyKit.xcframework") for SPM dev builds;
#   that path is gitignored and only materialized by
#   scripts/bootstrap-ghosttykit.sh (or fetched as @ghostty_kit via the
#   http_archive in MODULE.bazel for the real Bazel link). In a cold workspace
#   the artifact is absent, so `swift package describe` fails with
#   "GhosttyKit.xcframework does not contain binary artifact" and the analysis
#   aborts. This stub is only ever used to let SPM PARSE the manifest — the real
#   Bazel build links @ghostty_kit from the http_archive, never this path.
#
# Who calls it:
#   - CI: .buildkite/steps/{mac-app-build,integrity-bazel,boss-release}.sh
#   - Cube workspace provisioning: .cube/setup.yaml, so the deterministic
#     MODULE.bazel.lock resolver (which shells out to
#     `bazel mod deps --lockfile_mode=update`) works in a freshly provisioned
#     cold mono workspace instead of failing environmentally.
#
# Contract:
#   - Idempotent: a no-op if a framework (real or stub) is already present.
#   - Never destructive: it never overwrites an existing GhosttyKit.xcframework,
#     so a real bootstrapped framework is left untouched.
#   - Self-healing for a dangling symlink: a dev-bootstrapped workspace makes
#     ThirdParty/GhosttyKit.xcframework a symlink into the bazel repo cache. If
#     that cache entry is later GC'd, the symlink dangles: `-f
#     "${XCFW}/Info.plist"` reads false (so the idempotency guard doesn't
#     early-exit) but `mkdir -p "${XCFW}/..."` can't traverse the dead symlink
#     and fails with ENOENT. Detect that case and remove the stale symlink
#     before re-stubbing, rather than mkdir'ing through it.
#   - Safe off-macOS: on a host without the Apple toolchain (no `xcrun`, or not
#     Darwin) it prints a notice and exits 0 rather than failing. A cube setup
#     step that fails hard-fails the lease, and there is nothing this stub can
#     do on a non-macOS host anyway (the whole Swift/macOS toolchain is absent).
set -euo pipefail

# Resolve the app-macos directory relative to this script so the stub lands in
# the right place regardless of the caller's cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_MACOS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
XCFW="${APP_MACOS_DIR}/ThirdParty/GhosttyKit.xcframework"

if [[ -f "${XCFW}/Info.plist" ]]; then
  echo "[stub-ghosttykit] GhosttyKit.xcframework already present at ${XCFW}; nothing to do"
  exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]] || ! command -v xcrun >/dev/null 2>&1; then
  echo "[stub-ghosttykit] not a macOS host with an Apple toolchain (uname=$(uname -s), xcrun=$(command -v xcrun || echo none)); skipping stub"
  exit 0
fi

if [[ -L "${XCFW}" && ! -e "${XCFW}" ]]; then
  echo "[stub-ghosttykit] removing dangling GhosttyKit.xcframework symlink at ${XCFW}"
  rm -f "${XCFW}"
fi

echo "[stub-ghosttykit] creating GhosttyKit.xcframework stub for SPM describe at ${XCFW}"
mkdir -p "${XCFW}/macos-arm64"
cat > "${XCFW}/Info.plist" << 'PLIST_EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>AvailableLibraries</key>
    <array>
        <dict>
            <key>LibraryIdentifier</key>
            <string>macos-arm64</string>
            <key>LibraryPath</key>
            <string>GhosttyKit.a</string>
            <key>SupportedArchitectures</key>
            <array><string>arm64</string></array>
            <key>SupportedPlatform</key>
            <string>macos</string>
        </dict>
    </array>
    <key>CFBundlePackageType</key>
    <string>XFWK</string>
    <key>XCFrameworkFormatVersion</key>
    <string>1.0</string>
</dict>
</plist>
PLIST_EOF

STUB_OBJ="$(mktemp -t ghosttykit_stub.XXXXXX)"
trap 'rm -f "${STUB_OBJ}"' EXIT
printf 'void GhosttyKit_stub(void) {}\n' | \
  xcrun clang -arch arm64 -x c - -c -o "${STUB_OBJ}" -mmacosx-version-min=15.0
ar rcs "${XCFW}/macos-arm64/GhosttyKit.a" "${STUB_OBJ}"

echo "[stub-ghosttykit] ok"
