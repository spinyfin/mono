# Plan: Initial End-to-End PoC

## Goal

Get a minimal end-to-end proof of concept working: a native macOS app with a
basic chat interface, communicating through a Rust engine, which uses ACP to
interact with a single Claude Code agent.

The purpose is to validate the architecture and get something tangible running.
Scope is deliberately narrow -- single agent, single session, no persistence,
no fancy UI.

Protocol hardening and strict safety boundaries are explicitly out of scope for
this PoC.

## What "Done" Looks Like

1. User launches the macOS app.
2. User types a message in a chat input.
3. The message is sent to the Rust engine.
4. The engine forwards it to Claude Code via ACP.
5. Claude Code's streamed response flows back through the engine to the app.
6. The response appears in the chat view, streaming in as chunks arrive.

## Architecture (PoC)

```
┌──────────────────┐
│   macOS App      │
│   (SwiftUI)      │
│                  │
│  Chat View       │
│  Text Input      │
├──────────────────┤
│      ▲  │        │
│      │  ▼        │
│  Local IPC       │
│  (JSON over      │
│   Unix socket)   │
├──────────────────┤
│   Boss Engine    │
│   (Rust)         │
│                  │
│  Session mgmt    │
│  ACP client      │
├──────────────────┤
│      ▲  │        │
│      │  ▼        │
│  ACP (JSON-RPC   │
│  over stdio)     │
├──────────────────┤
│   Claude Code    │
│   (subprocess)   │
└──────────────────┘
```

## Components

### 1. Engine (Rust)

The engine is the core. For the PoC it needs to:

#### ACP Client

- Spawn the ACP agent as a subprocess. Claude Code does not have native ACP
  support; instead, Zed maintains an ACP adapter:
  [`@zed-industries/claude-code-acp`][claude-code-acp]. The engine spawns this
  adapter, which internally uses the Claude Agent SDK.
  - Command: `claude-code-acp` (installed via
    `npm install -g @zed-industries/claude-code-acp`)
  - Alternatively: `npx @zed-industries/claude-code-acp`
  - Environment: `ANTHROPIC_API_KEY` must be set.
  - Optional args: `--permission-mode <mode>` (default, acceptEdits,
    bypassPermissions, plan).
  - Debug: set `ACP_DEBUG=true` env var for debug logging to stderr.
- Implement the ACP client side of the protocol:
  - Send `initialize` with client capabilities.
  - Send `session/new` to create a session.
  - Send `session/prompt` with user messages.
  - Handle `session/update` notifications for streaming responses.
  - Handle agent-initiated requests: `fs/read_text_file`,
    `fs/write_text_file`, `terminal/create`, `terminal/output`,
    `terminal/wait_for_exit`, `terminal/kill`, `terminal/release`,
    `session/request_permission`.
- Transport: newline-delimited JSON-RPC 2.0 over stdin/stdout of the
  subprocess.

[claude-code-acp]: https://github.com/zed-industries/claude-code-acp

#### Frontend API

- Listen on a Unix domain socket for the frontend to connect.
- Simple JSON message protocol between engine and frontend:
  - Frontend → Engine: `{ "type": "prompt", "text": "..." }`
  - Engine → Frontend: `{ "type": "chunk", "text": "..." }` (streamed)
  - Engine → Frontend: `{ "type": "done" }`
  - Engine → Frontend: `{ "type": "tool_call", "name": "...", "status": "..." }`
  - Engine → Frontend: `{ "type": "permission_request", "id": "...", ... }`
  - Frontend → Engine: `{ "type": "permission_response", "id": "...", "granted": true }`
- Keep the IPC protocol intentionally simple for PoC speed; versioning and
  broader compatibility concerns are deferred.

#### PoC Scope

- Single agent (Claude Code).
- Single session (no multi-session management).
- No persistence (session lives as long as the engine runs).
- Working directory is the cwd where the engine is launched.
- Handle file system and terminal requests from Claude Code by executing them
  directly on the local machine (the engine acts as the ACP client, providing
  fs and terminal access) without additional sandboxing in this phase.

### 2. macOS App (SwiftUI)

A minimal native app. For the PoC:

#### UI

- A single-window app with a chat-style interface.
- Scrolling message list showing user messages and agent responses.
- Text input field at the bottom with a send button.
- Agent responses stream in (text appended as chunks arrive).
- Basic display of tool calls (e.g. "Reading file: /path/to/file") as
  inline status indicators.
- Permission request dialogs when the agent asks for approval.

#### Engine Communication

- On launch, start the engine as a subprocess (or connect to a running one).
- Communicate via Unix domain socket using the JSON protocol above.
- Parse streamed chunks and append to the current agent message in the UI.

#### PoC Scope

- No markdown rendering (plain text is fine).
- No syntax highlighting.
- No file diff views.
- No session history or persistence.
- No settings UI.

### 3. Claude Code Integration

- The engine spawns `claude-code-acp` (from `@zed-industries/claude-code-acp`)
  as a subprocess. This is the same adapter Zed uses. It bridges ACP to the
  Claude Agent SDK internally.
- `ANTHROPIC_API_KEY` must be set in the subprocess environment.
- The engine provides the ACP client-side handlers (fs, terminal, permissions).
- Use `pnpm` for Node package management in this repo.
- Use a pinned adapter version for reproducibility.
  - Preferred: add `@zed-industries/claude-code-acp@<pinned-version>` to a
    repo-managed `pnpm` workspace package and run via
    `pnpm exec claude-code-acp`.
  - One-off local testing: `pnpm dlx @zed-industries/claude-code-acp@<pinned-version>`.

## Build Strategy (PoC)

- Rust engine: use Bazel from day one.
- macOS app: use Xcode for initial PoC speed.
- Node tooling: use `pnpm` workspace conventions (no global `npm` installs).
- After PoC validation: add Bazel build support for the Swift app.

## Implementation Order

### Phase 1: Engine ACP Client

Build the Rust engine with ACP support.

1. **Project setup**: Create a Rust project under `tools/boss/engine/`. Set up
   Cargo.toml with dependencies: `tokio` (async runtime), `serde` / `serde_json`
   (JSON), `tracing` (logging).
2. **Startup preflight checks**: Validate required runtime prerequisites before
   connecting to ACP:
   - `ANTHROPIC_API_KEY` is present.
   - ACP adapter command is available and launchable.
   - On failure, return a clear startup error to the frontend/CLI and exit.
3. **ACP transport layer**: Implement newline-delimited JSON-RPC 2.0 reader/writer
   over stdin/stdout of a child process.
4. **ACP protocol**: Implement the client-side protocol state machine:
   initialize → session/new → prompt loop.
5. **Agent-initiated request handlers**: Implement handlers for fs/read_text_file,
   fs/write_text_file, terminal/create, terminal/output, terminal/wait_for_exit,
   terminal/kill, terminal/release, session/request_permission.
6. **Simple CLI test harness**: Before building the app, test the engine with a
   simple CLI that reads prompts from stdin and prints streamed responses. This
   validates ACP integration independently.

### Phase 2: Frontend API

Add the Unix socket server to the engine.

7. **Socket server**: Listen on a Unix domain socket, accept a single client
   connection.
8. **Message routing**: Wire up frontend messages to ACP prompt calls and ACP
   streaming updates to frontend messages.

### Phase 3: macOS App

Build the SwiftUI app.

9. **App skeleton**: Create a SwiftUI app with a single window, chat message
   list, and text input.
10. **Engine connection**: Connect to the engine's Unix socket, send/receive
   JSON messages.
11. **Streaming display**: Parse incoming chunks and update the chat view in
    real time.
12. **Tool call display**: Show basic tool call status inline in the chat.
13. **Permission handling**: Present permission requests as alerts, send
    responses back.

### Phase 4: Integration

14. **App launches engine**: Have the app start the engine subprocess on launch
    and connect to its socket.
15. **End-to-end test**: Send a prompt through the full stack, verify streamed
    response appears in the chat UI.

## Phase Acceptance Criteria

- **Phase 1**: From the CLI harness, one prompt produces streamed output chunks
  and a clean completion event.
- **Phase 2**: A socket client can send a prompt and receive `chunk` + `done`
  messages end-to-end through the engine.
- **Phase 3**: In the macOS app, agent output streams into the active message
  and at least one permission request can be accepted/denied.
- **Phase 4**: Launching the app starts the engine, sends a prompt, and returns
  a full streamed response without manual process setup.

## Open Questions

- **Engine lifecycle**: Should the app embed the engine in-process (as a Rust
  library via FFI) instead of running it as a subprocess? Subprocess is simpler
  for the PoC and maintains the clean separation, but in-process would reduce
  latency. Recommend subprocess for now.
- **Auth**: The `claude-code-acp` adapter requires `ANTHROPIC_API_KEY` to be
  set (it uses the Claude Agent SDK directly, not the Claude Code CLI's auth).
  The PoC will read this from the environment.

## Explicitly Out of Scope (PoC)

- Formal protocol hardening for engine ↔ frontend IPC (schema versioning,
  backward compatibility, multi-client guarantees).
- Strict safety boundary enforcement around ACP tool execution (sandboxing,
  policy engine). This is documented in the high-level design and deferred.

## Dependencies

### Rust (Engine)

- `tokio` - async runtime
- `serde`, `serde_json` - JSON serialization
- `tracing`, `tracing-subscriber` - structured logging

### Swift (macOS App)

- SwiftUI (built-in)
- Foundation (Unix socket via `NWConnection` or raw POSIX sockets)

### Node (ACP Adapter)

- `pnpm` for workspace package/dependency management in the monorepo
- `@zed-industries/claude-code-acp` pinned to a specific version

## Risks

- **ACP adapter maturity**: The `@zed-industries/claude-code-acp` adapter is
  maintained by Zed and used in production, but it's a bridge layer with its
  own potential quirks. Mitigation: start with the CLI test harness to isolate
  ACP issues from UI issues.
- **Streaming complexity**: Bidirectional async communication (engine ↔ Claude
  Code, engine ↔ frontend) adds complexity. Mitigation: use tokio for async
  and keep the message protocol simple.
- **Claude Code subprocess management**: Process lifecycle, crash recovery,
  stdin/stdout buffering. Mitigation: keep it simple for PoC -- no crash
  recovery, just clean startup/shutdown.
