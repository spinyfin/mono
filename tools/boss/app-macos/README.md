# Boss macOS App (PoC)

SwiftUI frontend for the boss PoC.

## One-command launcher

From repo root:

```bash
export ANTHROPIC_API_KEY=...
tools/boss/scripts/run-macos-poc.sh
```

Engine logs are written to `/tmp/boss-engine.log` by default (override with
`BOSS_ENGINE_LOG_PATH`).
Engine PID is written to `/tmp/boss-engine.pid` by default (override with
`BOSS_ENGINE_PID_PATH`).
Engine lifecycle events (start, every socket bind, shutdown — clean,
signalled, or panic) are appended as JSON lines to
`~/Library/Application Support/Boss/engine-audit.log` (override with
`BOSS_ENGINE_AUDIT_PATH`). The file lives outside `state.db` so it
survives db wipes; the engine rotates it in-place when it grows past
2 MiB. See [Forensic / audit log](#forensic--audit-log) below.
Internal system status messages are hidden by default. Set
`BOSS_SHOW_SYSTEM_MESSAGES=1` to show them in the chat transcript.

## Default flow (auto-launch engine)

Run the app and let it launch the engine automatically:

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/app-macos:Boss
```

By default the app launches:

```bash
bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-engine.sock
```

The engine runs from the workspace root.

When auto-start is enabled, the app will:

- reuse an existing engine process from the PID file when available,
- otherwise launch a new engine,
- relaunch an engine that exits unexpectedly with bounded exponential backoff,
- keep the engine running when the app exits (unless `BOSS_ENGINE_STOP_ON_EXIT=1`).

## External engine mode

Disable auto-start and point the app to an existing socket:

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-engine.sock
```

```bash
BOSS_ENGINE_AUTOSTART=0 BOSS_SOCKET_PATH=/tmp/boss-engine.sock bazel run //tools/boss/app-macos:Boss
```

## Agent capture (isolated UI screenshot)

Workers can screenshot the real Boss UI from a quiet isolated instance.
The capture path is in-process (`cacheDisplay` on a window that is never
ordered front) — no ScreenCaptureKit, no `screencapture`, no TCC grant,
no window on the operator's screen, no focus theft.

**Signal:** `BOSS_SOCKET_PATH` set to any path other than
`/tmp/boss-engine.sock`. That one env var drives the toolbar
`AGENT CAPTURE — isolated instance` badge, `.accessory` activation
policy (set in `applicationWillFinishLaunching`, never toggled), and a
per-instance `UserDefaults` suite (`dev.spinyfin.bossmacapp.capture`) so
window frames do not bleed into the operator's live app.

**Mode 1 — chrome / layout only (no engine):**

```bash
BOSS_SOCKET_PATH=/tmp/boss-shot-$ID.sock BOSS_ENGINE_AUTOSTART=0 \
  bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png
```

The app renders disconnected (`EngineUnreachableBanner` shows; board empty),
writes the PNG, and exits. Optional `--capture-after <seconds>` delays the
grab (default ~0.6s) so SwiftUI can finish first layout.

**Mode 2 — realistic UI with fixture state:** start an isolated engine first
(already permitted by the launch guard), seed a **fixture** DB (never copy
the operator's `state.db`), then capture:

```bash
env -u BOSS_EVENTS_SOCKET BOSS_WORKER_POOL_SIZE=0 \
  bazel run //tools/boss/engine/core:engine -- --socket-path /tmp/boss-shot-$ID.sock
# seed fixture rows via the boss CLI against that socket, then:
BOSS_SOCKET_PATH=/tmp/boss-shot-$ID.sock BOSS_ENGINE_AUTOSTART=0 \
  bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/shot.png --capture-after 3
```

**Launch-guard contract:** `bazel run` of an `app-macos` target is allowed
only when both `BOSS_SOCKET_PATH` is non-production and
`BOSS_ENGINE_AUTOSTART=0`. Direct `/Applications/Boss.app`, `open -a Boss`,
and bare binary launches remain blocked. The hard gate in
`agent_launch_guard.rs` is unchanged.

**Known capture limits (measured on macOS 26):**

- `NavigationSplitView` sidebar with `.listStyle(.sidebar)` loses **row text**
  under `cacheDisplay` (solid flat background). Detail pane, toolbar badge,
  and chrome survive. Sidebar layout bugs need a different verification path.
- Glass toolbar controls (FB20272917) may blank; the capture badge is a custom
  capsule so it stays legible.
- Agents-mode libghostty surfaces stay mounted at opacity 0 in Work mode;
  opacity-0 Metal layers do not blank the rest of the capture (measured).
- A **bare offscreen `NSHostingView`** (one never installed in an `NSWindow`)
  does not give its scroll views real scroll geometry: the nested scroll
  view's document view stays zero-width, and SwiftUI's
  `.onScrollGeometryChange` never fires, so any view that sizes itself from a
  scroll container's width lays out differently than it does in the app.
  Markdown tables are the case that bit us — see
  `MarkdownTableOverflowTests`, which hosts every case in a real `NSWindow`
  for exactly this reason. **Do not screenshot layout through a hand-rolled
  offscreen host**; use the `--capture-to` route above, which captures a real
  window, and if it fails say so rather than substituting an offscreen render.

Do not commit capture PNGs to the branch. Read the image back and state in
the PR body what you verified and what you could not.

## Overrides

- `BOSS_SOCKET_PATH`: unix socket path (default `/tmp/boss-engine.sock`)
- `BOSS_ENGINE_AUTOSTART`: set `0` to disable app-managed engine launch
- `BOSS_ENGINE_CMD`: custom command used when auto-start is enabled
- `BOSS_ENGINE_PID_PATH`: engine pid file path (default `/tmp/boss-engine.pid`)
- `BOSS_ENGINE_FORCE_RESTART`: set `1` to force-restart the engine on app launch
- `BOSS_ENGINE_STOP_ON_EXIT`: set `1` to stop engine when app exits
- `BOSS_ENGINE_RESTART_BACKOFF_SECONDS`: comma-separated restart delays in seconds (default `1,2,4,8,16,30`)
- `BOSS_ENGINE_RESTART_MAX_ATTEMPTS`: maximum automatic restart attempts before the app shows a manual-restart banner (default `6`)
- `BOSS_SHOW_SYSTEM_MESSAGES`: set `1` to include internal system status messages
- `BOSS_ENGINE_LOG_PATH`: log file path (default `/tmp/boss-engine.log`)
- `BOSS_ENGINE_AUDIT_PATH`: audit log file path (default
  `~/Library/Application Support/Boss/engine-audit.log`)
- `RUST_LOG`: tracing filter for engine logs (default `info`)

## Forensic / audit log

Every engine process appends one JSON line per lifecycle transition to
`~/Library/Application Support/Boss/engine-audit.log`:

- **`start`** — written before any work runs. Carries `pid`, `ppid`,
  `argv`, `parent_command` (best-effort `ps -o command=` of the
  parent), `engine_version`, `socket_paths` (the frontend and events
  sockets the engine _intends_ to bind), `state_db_path`, and
  `prior_state_db_size`.
- **`socket_bound` / `socket_bind_failed`** — emitted at each
  `UnixListener::bind` site. `socket_kind` is `frontend` or `events`.
  Failures include the formatted error.
- **`shutdown`** — written when a graceful-shutdown signal fires
  (`reason="signal:SIGINT"` / `signal:SIGTERM`), when `app::run`
  returns an error (`reason="error:<first line>"`), or when the
  process panics (`reason="crash:<first line>"`). Carries
  `uptime_sec` derived from the start record. A `start` line with no
  matching `shutdown` is itself the signal that the prior instance
  died unrecoverably (e.g. `SIGKILL`).

The file is bounded at ~2 MiB; on overflow the engine drops the
oldest half on the next append. Inspect it with `tail` or
`jq -c . engine-audit.log | tail`. To override the path (tests,
out-of-tree installs) export `BOSS_ENGINE_AUDIT_PATH`.

## Agents mode (Phase 6a libghostty embedding)

The toolbar's **Agents** mode is a full-bleed 4 × 2 grid of `libghostty`
terminal panes (one per worker slot). Each pane runs `claude` directly
via libghostty `initial_input` — engine-driven spawn lands in Phase 6f.

The Boss pane is intentionally not here; per the V2 design it lives in
**Work** mode as a docked panel (Phase 7). Phase 6a ships only the
worker grid.

### Bootstrap

`GhosttyKit.xcframework` is **not** checked in. Build it locally first:

```bash
cd tools/boss/app-macos
./scripts/bootstrap-ghosttykit.sh
```

The script clones `ghostty-org/ghostty`, builds the macOS xcframework via
zig, and places it at `tools/boss/app-macos/ThirdParty/GhosttyKit.xcframework`.

Requirements: macOS 15+, Xcode Metal Toolchain (`xcodebuild
-downloadComponent MetalToolchain`), and `zig@0.15` (Homebrew preferred,
falls back to a cached download).

### Run

Agents-mode panes are currently SwiftPM-only:

```bash
cd tools/boss/app-macos
swift run Boss
```

The Bazel build does not include `Sources/Ghostty/*.swift`; under Bazel
the Agents tab shows a placeholder pointing at this section. The Work
tab is unaffected and continues to function in both build paths.

### Known limitation: claude folder-trust prompt

Each pane launches `claude` in `$HOME` with no leased workspace yet, so
on first run claude shows its interactive workspace-trust prompt
("Accept" / "Cancel"). Click Accept once per pane the first time; the
acceptance is persisted in `~/.claude.json` and won't repeat on
subsequent app launches. This goes away in Phase 6f, where each worker
runs in a leased cube workspace the user already trusts.
