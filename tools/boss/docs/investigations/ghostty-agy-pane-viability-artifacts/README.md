# Artifacts: `agy` pane viability spike

The throwaway apparatus and the sanitized evidence behind [`ghostty-agy-pane-viability.md`](../ghostty-agy-pane-viability.md). Nothing here is Boss code and nothing here is built by Bazel.

- **Recorded:** 2026-11-11
- **Versions:** `agy` 1.1.12 and 1.5.5, GhosttyKit `ghosttykit-71c2d68e`, tmux 3.8

## Layout

| Path                                | What it is                                                                                                      |
| ----------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| `apparatus/ghosttykit_viewer.swift` | AppKit host that embeds one GhosttyKit surface with the calls the Boss app makes, and takes commands from files |
| `apparatus/tm`                      | The spike's private tmux server (`tmux -S`, never the default server and never Boss's)                          |
| `apparatus/pane.sh`                 | tmux control verbs with the argv `boss_tmux::Tmux` sends                                                        |
| `apparatus/common.sh`               | Scoped `HOME`, pre-seeding, launch, viewer, polling and wait helpers                                            |
| `apparatus/scenarios.sh`            | The scenarios, one function each                                                                                |
| `apparatus/dump_hook.sh`            | Hook command that records its stdin payload and environment, with a configurable reply                          |
| `apparatus/tools.py`                | `hooks.json` generator, launcher config, briefs, pane poller, sequence and marker analysis, key probe           |
| `evidence/01` … `13`                | One file per question, assembled from the run directories                                                       |

## What the apparatus is

A worker pane in Boss today is a tmux session that runs the driver under the initial-brief launcher, plus a GhosttyKit surface in the app whose shell runs `exec tmux -S <socket> attach-session -t <session>`. Each scenario builds exactly that:

1. A private tmux server with Boss's server and session options (`pane.sh server-start`, `pane.sh new`).
2. A session whose command exports the scoped `HOME` and execs either `agy` or Boss's **unmodified** `engine/driver/src/initial_prompt.py` wrapping `agy`.
3. `ghosttykit_viewer`, attached with the same initial input the app builds, polling `ghostty_surface_read_text(GHOSTTY_POINT_VIEWPORT)` every 250 ms.
4. Control through `tmux send-keys` / `load-buffer` + `paste-buffer -p`, as the engine does it.

The `iflag direct` scenario drops tmux and runs `agy` in the surface's own pty.

The viewer window is created off-screen and never ordered front, and the process runs with the accessory activation policy. It reads and types through GhosttyKit; it does not need to be visible.

The scripts are the ones that recorded the evidence, folded into fewer files afterwards. The viewer source differs from the binary that recorded it in two lines: its file writes now use the throwing `write(contentsOf:)`. The folded scripts and the rebuilt viewer were re-run on 1.5.5 (`launcher`, `esc`, `iflag`, `discovery`, `modes`, `signin`, `hideaccount`) and gave the same results.

## What it is not

It is not the Boss app and not a Boss dispatch. No engine code ran: no spawn confirmation, no events socket, no `boss-event`, no guard scripts. The launcher is the only Boss file executed.

## Running it

```sh
# 1. GhosttyKit at the MODULE.bazel pin (Bazel fetches and checksums it)
bazel query @ghostty_kit//:GhosttyKit
KIT="$(bazel info output_base)/external/+http_archive+ghostty_kit/GhosttyKit.xcframework/macos-arm64"

# 2. a scratch root; private copies of the binary under test in a read-only directory
export SPIKE_ROOT="$(mktemp -d)"
mkdir -p "$SPIKE_ROOT/viewer" "$SPIKE_ROOT/bin-1.5.5"
cp "$(command -v agy)" "$SPIKE_ROOT/bin-1.5.5/agy" && chmod 555 "$SPIKE_ROOT/bin-1.5.5"

# 3. the viewer
xcrun swiftc -swift-version 5 -O apparatus/ghosttykit_viewer.swift -I "$KIT/Headers" -L "$KIT" -lghostty-internal \
  -framework AppKit -framework Carbon -framework GameController -framework Metal -framework MetalKit \
  -framework QuartzCore -framework CoreText -framework CoreGraphics -framework IOKit -framework IOSurface \
  -framework UniformTypeIdentifiers -lc++ -o "$SPIKE_ROOT/viewer/ghosttykit_viewer"

# 4. briefs, tmux server, a scenario
python3 apparatus/tools.py briefs "$SPIKE_ROOT/briefs"
apparatus/pane.sh server-start
export BOSS_LAUNCHER="$PWD/../../../engine/driver/src/initial_prompt.py"
apparatus/scenarios.sh launcher 1.5.5 small
apparatus/scenarios.sh esc 1.5.5 tool
python3 apparatus/tools.py markers "$SPIKE_ROOT/runs" 1.5.5
apparatus/pane.sh kill-server
```

Every scenario needs a signed-in `agy` account on the host: the scoped `HOME` reaches the credential through a symlink to the host login keychain. Each turn spends that account's quota.

## Reading the evidence

- Timelines merge three clocks from one host: script marks, hook firings (stamped by the hook process) and `agy`'s own log. A script mark is stamped just after the action it names, so a keypress can appear some tens of milliseconds after the log line it caused.
- `<run:NAME>` is a scenario's run directory, `<spike>` the scratch root, `<home>` the real home directory, `<account-email>` the signed-in account.
- Pane excerpts drop blank lines and are cut at the right margin.

## Evidence index

| File                                        | Question                                                                   |
| ------------------------------------------- | -------------------------------------------------------------------------- |
| `01-versions-and-updater.txt`               | Binaries, checksums, how 1.1.12 became 1.5.5, what stops the updater       |
| `02-terminal-modes-and-submit-key.txt`      | Modes `agy` sets; bytes tmux delivers for `C-m` and `Enter`                |
| `03-startup-screens.txt`                    | Onboarding, trust, sign-in and start-up frames                             |
| `04-launcher-delivery.txt`                  | Brief delivery by the launcher, including the lost brief on 1.1.12         |
| `05-prompt-interactive-flag.txt`            | `-i` in a tmux pane and in a direct GhosttyKit pty                         |
| `06-hook-discovery-and-trust.txt`           | The discovery matrix and the trust-dialog timelines                        |
| `07-escape-interrupt.txt`                   | Escape during a tool call and during generation                            |
| `08-midturn-input-and-background-tasks.txt` | Queued input; `fullyIdle` across a background command                      |
| `09-hook-payloads.json`                     | One payload per event per version                                          |
| `10-hook-reply-semantics.txt`               | What each `PreToolUse` reply does; hook-terminated turns                   |
| `11-scoped-home.txt`                        | Credential location, state tree, tool and hook environment, real-home diff |
| `12-pane-markers.txt`                       | Marker presence per poll and phase; representative frames                  |
| `13-termination-reasons.txt`                | The enum in the binary against what was observed                           |

## Not kept

Run directories (conversation databases, transcripts, full `agy` logs, raw poll files), the compiled viewer, the GhosttyKit framework and the `agy` binaries. The full logs carry the account address and request trace ids; the excerpts here carry neither.
