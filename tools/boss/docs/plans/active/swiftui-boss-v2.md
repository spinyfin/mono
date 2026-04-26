# Boss SwiftUI V2 Plan

## Goal

Build a new version of Boss that keeps the strongest parts of the current
macOS SwiftUI shell, but replaces the Rust-engine-centered architecture with a
mostly Swift-native app that directly hosts and supervises embedded Ghostty
terminals.

The new system is centered around one "Boss" Claude session that coordinates a
fixed set of worker-agent Claude sessions. The Boss session should not do work
itself. Its job is planning, delegation, monitoring, and aggregation.

## Product Shape

### Core experience

- The main macOS app remains the primary surface.
- The `Agents` mode becomes a live control room built out of embedded Ghostty
  panes.
- There is one dedicated Boss Claude terminal and eight worker Claude
  terminals.
- Workers are shown in a fixed `2 x 4` tiled grid.
- The Boss session can inspect and control workers through a local control
  surface exposed by the app.

### What we keep from the current app

- SwiftUI app shell.
- Split-view information architecture.
- Segmented top-level mode switch (`Agents` / `Work`), unless we later decide
  the Boss control room needs its own top-level tab.
- Boss-specific framing in the UI, including status chips, selection state,
  and room for future work-management integration.

### What we remove or simplify

- The mandatory background Rust engine process.
- ACP as the core app architecture.
- The current socket protocol between frontend and engine.
- The current "chat transcript as the main primitive" approach for workers.

## Proposed V2 Architecture

### App layers

1. `BossMacApp`
   - SwiftUI shell, window management, app lifecycle.
2. `BossWorkspaceModel`
   - Shared observable app state for Boss session, worker sessions, selection,
     status, and command routing.
3. `GhosttyRuntime`
   - Shared embedded Ghostty runtime for all panes.
4. `TerminalSessionModel`
   - Per-pane state for terminal title, cwd, renderer health, Claude status,
     launch lifecycle, and control metadata.
5. `BossControlService`
   - Swift-native orchestration layer that exposes a command surface the Boss
     Claude instance can use to query and control workers.
6. `BossCommandBridge`
   - The concrete mechanism that makes the control surface available inside the
     Boss terminal session.

### Session layout

- `1` Boss session.
- `8` worker sessions.
- All sessions are embedded Ghostty surfaces in one app-owned runtime.
- Boss and workers are launched by the app, not by an external daemon.

### Initial UI layout

Use the current Boss app shell, but change the `Agents` detail area to a
control-room layout:

- top section: large Boss terminal
- secondary header: Boss status, worker summary, control status
- main section: fixed `2 x 4` grid of worker terminals
- optional side strip: selected worker details, alerts, or command results

This keeps the "main window interaction is with the Boss" requirement while
still making workers directly visible.

## Command and Control Surface

### Requirement

The Boss Claude instance needs a native way to query and control workers from
inside its terminal session, without depending on custom MCP installation.

### Preferred approach

Expose an app-owned local CLI command, for example `bossctl`, only to the Boss
session.

Capabilities should include:

- `bossctl agents list`
- `bossctl agents status`
- `bossctl agents focus <id>`
- `bossctl agents send <id> --text ...`
- `bossctl agents interrupt <id>`
- `bossctl agents transcript <id>`
- `bossctl agents launch <id>`
- `bossctl agents stop <id>`
- `bossctl workspace summary`

This can be implemented as:

- a small executable bundled by the app, or
- a shell script shim that talks to a local Unix domain socket / named pipe /
  loopback HTTP service exposed by the app

The key requirement is not "CLI" specifically. The key requirement is:

- Boss Claude can invoke it from the shell
- workers cannot invoke it
- it has structured, scriptable output

### Isolation model

Only the Boss session gets the control command on `PATH`.

Worker sessions should launch in an environment that:

- does not include the Boss control command
- does not include app-internal control credentials
- does not include authority to mutate sibling sessions directly

## Boss behavior contract

The Boss Claude instance must be bootstrapped with a strict operating contract:

- do not implement code directly
- do not edit files directly
- do not run project work yourself unless explicitly put into a fallback mode
- decompose work
- delegate work to workers
- monitor progress
- aggregate status
- ask the human for decisions when coordination or prioritization is ambiguous

This should be enforced in two places:

1. launch/bootstrap prompt for the Boss session
2. command-surface design that makes delegation easier than direct work

## State Model

### Per terminal session

- stable session id
- role: `boss` or `worker`
- display title
- working directory
- terminal readiness
- Claude presence
- Claude state: `starting`, `ready`, `working`, `awaiting_input`, `exited`
- renderer health
- last control action
- last observed activity timestamp
- last summary snippet

### Workspace state

- selected pane
- selected worker
- Boss health
- aggregate worker counts
- alerts / blocked workers
- command history
- app bootstrap state

## Monitoring Strategy

We should reuse what worked in the Ghostty prototype:

- screen-based detection of Claude readiness / working state
- prompt-region heuristics
- explicit detection of transient setup prompts such as workspace trust

But V2 should add a stronger side channel where possible:

- app-issued commands are tracked explicitly
- worker launch / interrupt / prompt-submit actions are recorded by the app
- the Boss control surface can return structured status independent of screen
  scraping

This means Ghostty observation remains useful for UI liveness, but operational
state should increasingly come from app-owned models.

## Migration Strategy

### Phase 1: shell-preserving rewrite

- keep the current SwiftUI shell and mode switch
- remove engine startup dependency from the macOS app
- introduce shared Ghostty runtime and embedded panes
- replace current `Agent` transcript model with terminal-session models
- show one Boss pane plus eight worker panes

### Phase 2: control surface

- implement `BossControlService`
- expose `bossctl` to the Boss session only
- support list/status/send/interrupt/focus operations
- log command activity in app state

### Phase 3: Boss bootstrap contract

- launch Boss Claude with a dedicated bootstrap prompt
- make Boss read the control reference on first launch
- verify Boss uses workers instead of doing work locally

### Phase 4: Work-mode reintegration

- reconnect the `Work` mode to the new Swift-native state model
- decide whether work tracking stays local-first, file-backed, SQLite-backed,
  or becomes a separate service later

## Open Design Questions

### 1. Where should the Boss terminal live?

Current recommendation:

- Boss gets the dominant area in `Agents`
- workers live below it in the `2 x 4` grid

Alternative:

- Boss gets its own mode or dedicated window

### 2. What is the control transport?

Candidates:

- bundled CLI over Unix domain socket
- bundled CLI over loopback HTTP
- direct file-based inbox/outbox queue

Current recommendation:

- bundled CLI over Unix domain socket with JSON output

### 3. How strict should worker isolation be?

Current recommendation:

- soft isolation first: PATH/env separation only
- stronger isolation later if needed

### 4. Should workers be fixed or dynamic?

You asked for a fixed initial shape. Current recommendation:

- V2 starts with exactly eight workers
- dynamic worker counts can come later

## First Implementation Slice

The first slice should prove the new architecture with the least moving parts:

1. keep the current Boss app shell
2. replace `Agents` detail with one Boss Ghostty pane plus an `8`-worker grid
3. auto-launch Claude in all panes
4. provide a minimal app-owned `bossctl` with:
   - `agents list`
   - `agents status`
   - `agents send`
5. give `bossctl` only to the Boss pane
6. bootstrap Boss with "coordinate only; never do work directly"

If that works, we can then decide whether the rest of the old Rust engine
should be retired completely or whether any subset of it is still worth keeping.
