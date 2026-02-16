# Boss macOS App (PoC)

SwiftUI frontend for the boss PoC.

## One-command launcher

From repo root:

```bash
export ANTHROPIC_API_KEY=...
tools/boss/scripts/run-macos-poc.sh
```

Use `tools/boss/scripts/run-macos-poc.sh --skip-install` to skip `pnpm install`.
Engine logs are written to `/tmp/boss-engine.log` by default (override with
`BOSS_ENGINE_LOG_PATH`).
Engine PID is written to `/tmp/boss-engine.pid` by default (override with
`BOSS_ENGINE_PID_PATH`).

## Default flow (auto-launch engine)

Run the app and let it launch the engine automatically:

```bash
cd tools/boss/app-macos
ANTHROPIC_API_KEY=... swift run BossMacApp
```

By default the app launches:

```bash
bazel run //tools/boss/engine:engine -- --mode=server --socket-path /tmp/boss-engine.sock
```

When auto-start is enabled, the app will:

- reuse an existing engine process from the PID file when available,
- otherwise launch a new engine,
- keep the engine running when the app exits (unless `BOSS_ENGINE_STOP_ON_EXIT=1`).

## External engine mode

Disable auto-start and point the app to an existing socket:

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/engine:engine -- --mode=server --socket-path /tmp/boss-engine.sock
```

```bash
cd tools/boss/app-macos
BOSS_ENGINE_AUTOSTART=0 BOSS_SOCKET_PATH=/tmp/boss-engine.sock swift run BossMacApp
```

## Overrides

- `BOSS_SOCKET_PATH`: unix socket path (default `/tmp/boss-engine.sock`)
- `BOSS_ENGINE_AUTOSTART`: set `0` to disable app-managed engine launch
- `BOSS_ENGINE_CMD`: custom command used when auto-start is enabled
- `BOSS_ENGINE_PID_PATH`: engine pid file path (default `/tmp/boss-engine.pid`)
- `BOSS_ENGINE_FORCE_RESTART`: set `1` to force-restart the engine on app launch
- `BOSS_ENGINE_STOP_ON_EXIT`: set `1` to stop engine when app exits
- `BOSS_ENGINE_LOG_PATH`: log file path (default `/tmp/boss-engine.log`)
- `RUST_LOG`: tracing filter for engine logs (default `info,acp_stderr=debug`)
