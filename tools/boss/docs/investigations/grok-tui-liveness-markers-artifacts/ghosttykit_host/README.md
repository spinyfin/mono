# GhosttyKit Grok liveness-marker host (throwaway)

Minimal AppKit process that embeds libghostty via the **same APIs Boss uses**
(`ghostty_surface_new` / `ghostty_surface_read_text` / `ghostty_surface_text` +
Return). Not standalone Ghostty.app.

Captures every viewport change under three pane modes so TUI liveness markers
can be measured for stability (not one-off presence).

## Setup

```sh
# pin ghosttykit-5659cef (same as MODULE.bazel @ghostty_kit)
# sha256 82b8d947484a21e1a9d186628b8af5e3f2e81dc96925f3cdbc1766ececa814a1
ln -sfn /path/to/GhosttyKit-5659cef.xcframework .local-GhosttyKit.xcframework
```

Isolated `GROK_HOME=/tmp/grok-liveness-spike/home` (auth + trusted_folders only —
never the operator's real `~/.grok` as runtime home).

## Run

```sh
export GROK_HOME=/tmp/grok-liveness-spike/home
export SPIKE_CWD=/tmp/grok-liveness-spike/cwd
export SPIKE_PANE_MODE=no_alt    # or minimal | default
./run.sh
# evidence lands in run_out/; copy into ../evidence/<mode>/
```
