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
GHOSTTY_REF=<FULL_40_HEX_COMMIT> bash scripts/bootstrap-ghosttykit.sh
```

The script fetches and checks out the named immutable Ghostty commit from `https://github.com/ghostty-org/ghostty`, builds the `GhosttyKit.xcframework` (static, arm64, `-Doptimize=ReleaseFast`), and places it at `ThirdParty/GhosttyKit.xcframework`. When `GHOSTTY_REF` is unset it reads the full commit from the `# ghostty_kit_commit = …` line in monorepo-root `MODULE.bazel` (and checks that it matches the `ghosttykit-<short>` prebuilt URL prefix in the same file). Set `GHOSTTY_REF` to another full commit ID only when deliberately building a revision not yet pinned in `MODULE.bazel`. Do not use a moving branch such as `main`.

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

Edit the `ghostty_kit` pin near the bottom of `MODULE.bazel` — **both** the full-commit comment (SwiftPM bootstrap default) **and** the `http_archive` URL/sha256 (Bazel/CI). They must agree on the same commit; `bootstrap-ghosttykit.sh` refuses to build if the short prebuilt tag does not prefix the full commit.

```python
# Full ghostty commit for SwiftPM bootstrap (tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh
# parses this line). Must match the ghosttykit-<short> prefix in the URL below.
# ghostty_kit_commit = <FULL_40_HEX_COMMIT>
http_archive(
    name = "ghostty_kit",
    urls = ["https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-<NEW_SHORT_SHA>/GhosttyKit-<NEW_SHORT_SHA>.tar.gz"],
    sha256 = "<NEW_SHA256>",
    build_file = "//tools/boss/app-macos:ghosttykit.BUILD",
)
```

Do not leave a separate hard-coded default in `tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh` — that script derives its pin from this block.

### 5. Verify locally

```sh
bazel build //tools/boss/app-macos:Boss
# Optional: confirm SwiftPM bootstrap resolves the same pin without GHOSTTY_REF=
bash -n tools/boss/app-macos/scripts/bootstrap-ghosttykit.sh
```

Bazel should fetch the new archive, compile `Sources/Ghostty/*.swift`, and produce `Boss.app` with Workers mode functional.

### 6. Open a PR

Open a PR with the `MODULE.bazel` pin change (title: `chore: bump GhosttyKit to <GHOSTTY_SHA>`). The commit must update the `ghostty_kit_commit` line and the `http_archive` URL/sha256 together; `bootstrap-ghosttykit.sh` needs no pin edit. CI runs `bazel build //tools/boss/installer/...` and `bazel test //tools/boss/engine/...`.

## Notes

- The arm64-only xcframework is intentional — Boss targets arm64 Macs only.
- The tarball is ~45 MB; Bazel caches it after the first fetch.
- `bootstrap-ghosttykit.sh` is only needed for SwiftPM dev builds and for generating new prebuilt releases. The Bazel installer build never calls it.
- Ghostty pin source of truth is `MODULE.bazel` only (full commit comment + prebuilt URL); the bootstrap script and this runbook must not carry a second hard-coded default.
