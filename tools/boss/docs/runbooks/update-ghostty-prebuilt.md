# Runbook: Update prebuilt GhosttyKit.xcframework

Run this when Ghostty needs a version bump or the xcframework must be rebuilt.

## Prerequisites

- macOS machine with Xcode and the Metal Toolchain component installed
- `xcodebuild -downloadComponent MetalToolchain` if not already present
- `gh` CLI authenticated as `spinyfin`
- Zig 0.16.0 (the bootstrap script will download it if not found)

## Steps

### 1. Build the xcframework

```sh
cd tools/boss/app-macos
GHOSTTY_REF=71c2d68eb40d4e51d30d94e46e7b0c305aa4407f bash scripts/bootstrap-ghosttykit.sh
```

The script fetches and checks out the named immutable Ghostty commit from `https://github.com/ghostty-org/ghostty`, builds the `GhosttyKit.xcframework` (static, arm64, `-Doptimize=ReleaseFast`), and places it at `ThirdParty/GhosttyKit.xcframework`. It defaults to `71c2d68eb40d4e51d30d94e46e7b0c305aa4407f`; set `GHOSTTY_REF` to another full commit ID when deliberately rebuilding a different revision. Do not use a moving branch such as `main`.

Note the ghostty commit SHA:

```sh
git -C .build-cache/ghostty-upstream rev-parse --short HEAD
# e.g. b0f827665
```

### 2. Create the release tarball

```sh
GHOSTTY_SHA=$(git -C tools/boss/app-macos/.build-cache/ghostty-upstream rev-parse --short HEAD)

tar -czf "GhosttyKit-${GHOSTTY_SHA}.tar.gz" \
  -C tools/boss/app-macos/ThirdParty GhosttyKit.xcframework

shasum -a 256 "GhosttyKit-${GHOSTTY_SHA}.tar.gz"
```

Record the SHA256 — you will need it in step 4.

### 3. Publish to spinyfin/ghostty-prebuilts

```sh
gh release create "ghosttykit-${GHOSTTY_SHA}" \
  --repo spinyfin/ghostty-prebuilts \
  --title "GhosttyKit ${GHOSTTY_SHA}" \
  --notes "Built from ghostty commit ${GHOSTTY_SHA}. SHA256: <sha256 from step 2>" \
  "GhosttyKit-${GHOSTTY_SHA}.tar.gz"
```

### 4. Update MODULE.bazel in mono

Edit the `http_archive` block near the bottom of `MODULE.bazel`:

```python
http_archive(
    name = "ghostty_kit",
    urls = ["https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-<NEW_SHA>/GhosttyKit-<NEW_SHA>.tar.gz"],
    sha256 = "<NEW_SHA256>",
    build_file = "//tools/boss/app-macos:ghosttykit.BUILD",
)
```

### 5. Verify locally

```sh
bazel build //tools/boss/app-macos:Boss
```

Bazel should fetch the new archive, compile `Sources/Ghostty/*.swift`, and produce `Boss.app` with Workers mode functional.

### 6. Open a PR

Open a PR with the MODULE.bazel change (title: `chore: bump GhosttyKit to <GHOSTTY_SHA>`). CI runs `bazel build //tools/boss/installer/...` and `bazel test //tools/boss/engine/...`.

## Notes

- The arm64-only xcframework is intentional — Boss targets arm64 Macs only.
- The tarball is ~45 MB; Bazel caches it after the first fetch.
- `bootstrap-ghosttykit.sh` is only needed for SwiftPM dev builds and for generating new prebuilt releases. The Bazel installer build never calls it.
