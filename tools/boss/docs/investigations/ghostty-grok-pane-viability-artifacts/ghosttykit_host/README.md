# GhosttyKit Grok spike host (throwaway)

Minimal AppKit process that embeds libghostty via the **same APIs Boss uses**
(`ghostty_surface_new` / `ghostty_surface_read_text` / `ghostty_surface_text` +
`ghostty_surface_key` Return/Esc). Not standalone Ghostty.app.

## Setup

```sh
curl -fsSL -o /tmp/GhosttyKit-5659cef.tar.gz \
  "https://github.com/spinyfin/ghostty-prebuilts/releases/download/ghosttykit-5659cef/GhosttyKit-5659cef.tar.gz"
# sha256 must be 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
tar -xzf /tmp/GhosttyKit-5659cef.tar.gz
# link or move GhosttyKit.xcframework → .local-GhosttyKit.xcframework
```

## Run

```sh
export GROK_HOME=/tmp/grok-pane-spike/home   # isolated home with auth + trusted_folders
export SPIKE_SCENARIO=seed_observe           # or esc_interrupt | resize | alt_screen
./run.sh
# evidence snapshot lands under evidence/<scenario>/
```

Pinned prebuilt: `ghosttykit-5659cef` (same as MODULE.bazel `@ghostty_kit`).
