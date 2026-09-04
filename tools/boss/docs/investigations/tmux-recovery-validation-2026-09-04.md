# Tmux recovery validation — 2026-09-04

## Apparatus

- Repository target: `bazel test //tools/boss/engine/core:tmux_recovery_integration_test`
- Supported drivers installed on the validation host: Claude Code `2.1.260` and Codex CLI `0.150.0`.
- App capture command: `BOSS_SOCKET_PATH=/tmp/boss-recovery-capture-18d1b968.sock BOSS_ENGINE_AUTOSTART=0 bazel run //tools/boss/app-macos:Boss -- --capture-to /tmp/boss-recovery-capture-18d1b968.png`.

## Production tmux-path integration result

The Bazel target passed on the validation host. It creates a temporary SQLite
state store and production `Tmux` controller against a private socket, then
enters through `start_worker` with the durable `WorkDb` spawn store. Its
login-shell fixture continuously redraws the pane and changes the terminal
title to `boss-repainting-fixture-title`; it never emits a driver event.

The target verifies this sequence with a one-second test-only timing window:

1. local tmux spawn persists the token and session identity before the pane
   becomes observable, then requests an attached viewer;
2. a fresh engine-local registry adopts that session from the private server;
3. terminal inspection still reports a live pane whose foreground command is
   not `claude`, while window activity advances from repainting;
4. only recorded semantic idle state makes the execution stale, after which
   token-verified teardown removes the tmux session;
5. the sweep releases the worker-pool slot and cube lease; and
6. the now-orphaned auto-start chore receives a fresh ready execution through
   the production orphan-redispatch path.

The run also found and exercised the real bootstrap requirement that tmux must
receive `start-server`, `exit-empty=off`, and first-window defaults in one
command sequence. A standalone `start-server` exits an empty private server
before a later client can set its defaults.

## Isolated app and supported-driver drill record

Two isolated app captures were attempted: the command above and the same
command with `--capture-after 3`. Both built the application successfully, but
the capture executable exited with `Boss capture: capture failed: NSApp.windows
is empty (no WindowGroup window created)` after its internal retry loop.

Consequently, neither Claude nor Codex reached an attached real-app pane in
this environment. The requested engine-restart, app-restart, normal-exit
(`pane_dead` plus exit status), and controlled stale-idle observations therefore
have no passing result recorded here. The integration target above remains the
repeatable recovery evidence; it is not substituted for the supported-driver
drill. A host where the isolated app can create its capture window must rerun
the two supported drivers and append their restart, exit-status, and
controlled-recovery observations to this document.
