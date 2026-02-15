# Boss macOS App (PoC)

SwiftUI frontend for the boss PoC.

## 1) Start the engine server

```bash
ANTHROPIC_API_KEY=... bazel run //tools/boss/engine:engine -- --mode=server --socket-path /tmp/boss-engine.sock
```

## 2) Start the app

```bash
cd tools/boss/app-macos
BOSS_SOCKET_PATH=/tmp/boss-engine.sock swift run BossMacApp
```

The app sends prompts to the engine over the Unix socket and renders streamed chunks.
In this commit, permission requests are auto-allowed by the frontend.
