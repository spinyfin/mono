# GhosttyKit embed harness (throwaway)

Minimal AppKit host that embeds **GhosttyKit / libghostty** the way Boss does
(`ghostty_surface_new` + `nsview`, observe via `ghostty_surface_read_text`,
inject via `ghostty_surface_text` + `ghostty_surface_key` Return).

This is **not** standalone Ghostty.app + outsider `shell_pid` observation.

## Pins

- GhosttyKit prebuilt: `ghosttykit-5659cef` from `spinyfin/ghostty-prebuilts`
  (same pin as `MODULE.bazel` `@ghostty_kit`)
- `codex-cli` from `CODEX_BIN` or `~/.local/bin/codex`

## Setup

The xcframework is **not** committed (≈140 MB). Materialize it next to this
README:

```sh
cd tools/boss/docs/investigations/ghostty-codex-pane-viability-artifacts/ghosttykit_host
curl -fsSL -o /tmp/GhosttyKit-5659cef.tar.gz \
  "https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-5659cef/GhosttyKit-5659cef.tar.gz"
# sha256 must match MODULE.bazel @ghostty_kit
shasum -a 256 /tmp/GhosttyKit-5659cef.tar.gz
# expected: 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
rm -rf .local-GhosttyKit.xcframework
tar -xzf /tmp/GhosttyKit-5659cef.tar.gz
mv GhosttyKit.xcframework .local-GhosttyKit.xcframework
```

## Build + run

Requires a real macOS GUI session (AppKit + Metal).

```sh
./run.sh
# artifacts land in ./run_out/
```

## What it answers

| Question | How                                                                                                              |
| -------- | ---------------------------------------------------------------------------------------------------------------- |
| Q1 embed | Poll `ghostty_surface_read_text` for `thread.started` / `gkit-embed-done` while `codex exec` runs in the surface |
| Q2 embed | Mid-run + post-exit inject via Boss-equivalent `submitText` path; side-effect files prove shell consumption      |

See the parent investigation doc for dual-topology interpretation.
