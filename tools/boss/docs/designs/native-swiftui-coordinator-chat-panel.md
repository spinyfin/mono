# Native SwiftUI coordinator chat panel: the coordinator becomes a headless Claude Code session driven over ACP

- **Date:** 2026-11-23
- **Status:** Design for review. The validating spike has run and its gate passed, with conditions; see [Spike result](#spike-result). Nothing after the spike has been built.
- **Scope:** the coordinator only (`tools/boss/app-macos`, `tools/boss/engine`, `tools/boss/protocol`). Workers stay terminal panes.
- **Source baseline:** `6df8745bd093` on `main`. Code claims come from reading that revision. Paths are relative to `tools/boss/` unless they start with `tools/` or name a root file.
- **External sources:** the Agent Client Protocol documentation, the Claude Code ACP adapter's repository, and Anthropic's Claude Code documentation, all read on 2026-11-23 and listed under [External sources](#external-sources).
- **What was observed at runtime.** The spike ran on 2026-11-25 against the real adapter, `claude` and login. Sections it changed say so and cite [coordinator-acp-spike.md](../investigations/coordinator-acp-spike.md). Everything about the engine service, the agent host and handoff is still unobserved.

The coordinator pane stops being an embedded terminal and becomes a native chat panel. The contested bet is the backend: **the coordinator stops being an interactive Claude Code terminal session and becomes a headless Claude Code session that the engine drives over the Agent Client Protocol (ACP) through an adapter Boss does not own**, so Boss gains structure and takes on the conversation surface it has inherited from the terminal UI since April. The terminal coordinator stays the default until the parity list here is met.

## Verdict

Recommend option (b), ACP, with five decisions that shape everything else:

1. **The ACP client lives in the engine, not the app.** The engine already owns coordinator creation, restart and handoff, and the coordinator must keep working while the app is closed. The app is a view over an engine-held conversation.
2. **tmux stays as the process host; only the interface changes.** The coordinator's tmux session keeps its name, spawn token, supervisor and restart ceiling. Its pane runs a small byte relay (the agent host) with the ACP adapter underneath, instead of `claude`. An engine or app restart therefore still loses nothing, which is the property the tmux project was built to provide.
3. **The part that survives upgrades stays small and frozen.** The agent host forwards lines and journals them. All protocol knowledge sits in the engine, which rebuilds its state from that journal and from its own log of what it intended to send.
4. **Handoff keeps its protocol and changes only its transport.** The brief, the reset request, the authorization redelivery, their ids, the verify step, the timeouts and the `boss handoff` commands are unchanged. They travel as ACP prompts instead of the terminal relay and Claude Code's local inbox. Message text changes only where it names the inbox.
5. **The engine-to-app vocabulary is Boss's own, not ACP's.** If the spike rejects the adapter, the first fallback (Claude Code's own stream-json mode) is confined to the client crate, the reducer's mapping and one new component, a permission-prompt tool. The agent host, the journal, the queue, the RPC surface and the app keep their shape.

The spike is a **validating** study of (b), not a choice between options. The choice is made in this document on paper, with the evidence that would reopen it named in advance. Seventeen entries: fourteen in scope, three deferred.

## Status and evidence gaps

This section is perishable. Refresh it as entries land; the reasoning in later sections should not need to change with it.

Refreshed on 2026-11-25, after the spike. Evidence for each line is in [coordinator-acp-spike.md](../investigations/coordinator-acp-spike.md).

- The adapter at version 0.110.0 was run, with Claude Code 2.1.332 and with its own 2.1.330. All eight required spike criteria passed.
- The coordinator's instructions, allow rules and memory load under the adapter (S2), provided the session directory is not inside Claude Code's own temporary root.
- A background agent's completion has a start and an end a client can rely on only when the client advertises the adapter's `backgroundSubagents` and `asyncTasks` capabilities (S6). Those signals are an adapter extension, outside ACP v1 and outside the official Rust schema.
- Claude Code's cross-session inbox does exist in an adapter-hosted session, and a peer message is answered (S10).
- How a headless coordinator's usage is metered is stated by Anthropic's help centre today, but that policy changed twice during 2026. The spike's sessions reported the plan's seven-day window (S8). Whether that is acceptable is still the repo owner's decision. See [Usage metering and account policy](#usage-metering-and-account-policy).
- The isolation gap described in the findings below was closed by mono#3920 while the spike ran. The spike did not depend on it: it rendered its own session directory and set the engine socket itself, and it did not run an isolated engine with a registered app.
- Still unobserved: the agent host, journal replay, the intent log, handoff delivery, engine and app restarts, and any session longer than a few turns.

## Findings that shape the design

These came out of reading the current code. Several are places where no decision was ever recorded; they are treated as never decided, not as decisions whose reasons were lost.

- **The coordinator has no structured signal today.** No hooks are wired for it (the `boss-event` hooks are worker-only, `engine/driver/src/claude.rs`). The engine has no busy or idle state for it, no compaction detection and no `/clear` detection. The only turn-end observation is `TurnObserver` in `engine/core/src/coordinator_tmux/inbox_turn.rs`, which reads the transcript during an inbox delivery.
- **`bossctl agents transcript`, `focus` and `interrupt` have never addressed the coordinator.** They resolve a worker run (`bossctl/src/agents.rs:1751` sends `TailRunTranscript { run_id }`; `engine/app/src/app/pane_ops.rs:424` `interrupt_worker_pane`). The coordinator has no run row. There is nothing to preserve, and this design does not add coordinator support to them.
- **The engine pushes only four kinds of text into the coordinator.** The session-start brief, the prompt-changed nudge, the reset request and the handoff authorization. There is no push for worker completion, attention items or merges; the coordinator learns those by running `boss` and `bossctl`.
- **Other Claude Code sessions can reach the coordinator, and nobody decided they should.** The coordinator launches with `--settings '{"crossSessionInbound":"accept"}'` (`engine/core/src/coordinator_tmux/inbox.rs:97`). The recorded reason is that Boss's own deliveries must not depend on the user's inbound policy. That any same-user session can also put text in front of the coordinator is recorded as a cost ([coordinator-session-handoff.md](../coordinator-session-handoff.md), "Trust boundary"), not as a wanted feature.
- **Image paste is not a Boss feature.** The terminal's clipboard bridge is text only (`app-macos/Sources/Ghostty/GhosttyRuntime.swift:494`), and nothing in the engine or the app carries an image to the coordinator. Screenshots can reach it only through the Claude Code terminal UI's own clipboard handling, which Boss neither provides nor tests.
- **Work-item links do not exist today.** The terminal shows ids as plain text, and the coordinator runs `bossctl reveal` to show a card. Links in the chat panel are a new capability, not parity.
- **The app renders the system prompt, not the engine.** `bossSystemPrompt` is written to `boss-session/CLAUDE.md` on every app start (`app-macos/Sources/Ghostty/BossPaneModel.swift:125-159`). A coordinator restarted while the app is closed uses whatever the last app launch wrote. This design leaves that as it is.
- **A coordinator started by a non-production engine does not look isolated.** The session directory is a fixed path under Application Support on both sides: `coordinator_working_directory` (`engine/core/src/coordinator_tmux.rs:1322`), whose only override is a test seam (`coordinator_session_directory` in `engine/app/src/app.rs`), and `ensureBossWorkingDirectory` in the app (`BossPaneModel.swift:125-132`). The session environment (`coordinator_tmux.rs:1061-1071`) does not carry the engine's socket path, which is what the `boss` CLI reads to find its engine (`client/src/lib.rs:100`). The tmux socket, by contrast, is derived per engine. Any entry here that launches a coordinator under an isolated engine depends on closing this first.
- **The April decision record said moving to native chat later would be "a frontend change".** It is not. The coordinator's launch, brief delivery, handoff transport, trust root and update detection all assume a terminal program in a pane. That is most of this design.

## Goals

- Replace the coordinator pane in Work mode, today a Claude Code session in tmux shown through an embedded Ghostty terminal, with a native SwiftUI chat panel backed by a structured agent protocol.
- Phase one is purely conversational, at parity with today: the user types, the coordinator answers, and tool activity and results stay visible.
- Tool calls, permission prompts and the auto-mode classifier's decisions are visible in the panel, and a permission prompt can be answered there.
- Preserve what the coordinator gets from being a real Claude Code session: its instructions, allow rules, auto mode, memory, background agents, handoff, and survival across app and engine restarts.
- Leave room for later phases (structured cards, inline actions) without building them.
- Keep the terminal coordinator as the default until parity is demonstrated, with a planned switch-over and a fallback.

"Parity" means the list in [Parity list](#parity-list). It does not mean every affordance of the Claude Code terminal UI; the affordances that phase one drops are listed in [Terminal affordances and their disposition](#terminal-affordances-and-their-disposition).

## Non-goals

- Changing workers. They remain terminal panes with hooks.
- Structured cards, inline actions, or any Boss-specific rendering of a tool's result. Tool calls get one generic row.
- Carrying conversation context across a coordinator restart. A restarted coordinator is a fresh conversation, as it is today; the handoff brief remains the continuity mechanism. `session/resume` is not wired.
- A second agent driver for the coordinator. See [Model and driver](#model-and-driver).
- A Boss-rendered slash-command, model or MCP picker, plan-mode UI, or in-app Claude login.
- An automatic fallback to the terminal backend. See [Switch-over and fallback](#switch-over-and-fallback) for why.
- Raising an attention item when the coordinator waits on a permission request.
- A coordinator variant of `bossctl agents transcript` or `focus`.
- Moving the system prompt out of `CLAUDE.md`, or changing its content beyond wording that names the terminal or the inbox.
- Any `state.db` schema change. The design uses the existing `metadata` key/value table and files.
- Multi-window behaviour of the panel. That belongs to [native window tabs](native-window-tabs-with-tear-off.md).

## Where ACP was before

Boss started on ACP. The history, from the repository:

| Merged     | Change                                                                                                                                                                        |
| ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 2026-02-16 | mono#2 adds `tools/boss/engine/src/acp.rs` and a CLI harness. Design: [initial-poc.md](../plans/done/initial-poc.md).                                                         |
| 2026-02-17 | mono#13 and mono#14 add multiple agents: one adapter subprocess per agent, an `AgentRegistry`, a sidebar.                                                                     |
| 2026-04-29 | [v2-design-risks.md](v2-design-risks.md) risk R1 decides terminal-embed over native chat. [main.md](main.md) is rewritten around it.                                          |
| 2026-05-06 | mono#163 replaces the chat-style Boss panel in Work mode with a libghostty pane running `claude`.                                                                             |
| 2026-05-08 | mono#294 deletes the ACP transport, the agent registry, the chat RPCs and events, the app's chat state and composer, and the npm adapter wrapper (about 4,200 lines removed). |

**What it did.** `acp.rs` was a hand-written JSON-RPC client, 1,404 lines. It spawned `@zed-industries/claude-code-acp` from npm, spoke protocol version 1, and sent text-only prompts. It advertised the `fs` and `terminal` client capabilities, so the engine itself read and wrote files and ran commands on the model's behalf (`ClientHost` and `TerminalManager`, roughly half the file). It had a permission coordinator, no `session/load`, no cancel and no images. The protocol carried `Chunk`, `Done`, `ToolCall`, `TerminalStarted`/`Output`/`Done` and `PermissionRequest` events to a SwiftUI message list and composer. The PoC plan records that the adapter of the time needed `ANTHROPIC_API_KEY`, not the Claude login.

**Why it was replaced.** The recorded decision (R1) gives three reasons: terminal-embed inherits Claude Code's whole product surface for free; reversing from terminal to chat is cheaper than the other way; and Boss's own session benefits from being a real Claude terminal. It also recorded two things that matter now. It named the "inside-the-conversation" affordances (custom permission UI, collapsible tool calls, inline work-item links, sender provenance) as "the explicit forcing function for revisiting this risk". And [main.md](main.md) adopted a principle: "Boss runs the unmodified `claude` binary … No custom Claude tool handlers."

**What is reusable.**

- Nothing remains in the tree. The old code is readable at the parent of commit `36b7ff7c6b3e`.
- Do not resurrect `acp.rs`. An official Rust crate (`agent-client-protocol`) now provides the typed schema. The old file remains a useful reference for newline-delimited framing and the pending-request table, about 300 lines.
- Do not resurrect the `fs` and `terminal` handlers. They are what made Boss a custom tool executor. This design advertises neither capability, so Claude Code runs every tool itself and the main.md principle holds.
- The shape of the permission coordinator (register a pending request, resolve it by id from a later RPC) carries over.
- The harness-first approach carries over: the initial PoC plan said to "start with the CLI test harness to isolate ACP issues from UI issues". The spike does that.
- In the app, the old composer and chunk batcher are recoverable as reference. The markdown renderer they used, Textual, is back in the app at 0.3.1 with a full Boss theme (`app-macos/Sources/BossMarkdownStyle.swift`).

**What changed since April** that makes revisiting reasonable: the adapter now runs on the user's Claude login and can run the installed `claude` binary; it exposes auto mode, images, background tasks and compaction; an official Rust client crate exists; Boss has engine-owned coordinator lifecycle and a handoff, so "we own resume entirely" is already paid for; and the scope is one conversation, not the whole worker fleet.

## Alternatives considered

### Backend

**(a) Type into the tmux pane and parse the screen.** Rejected. Boss has no precedent for reading conversation state from the screen, and the tmux design lists "driving the agent through `send-keys` as a control API" as a non-goal. Boss does type into panes (worker prompt delivery, the coordinator's prompt-changed nudge), but those are single deliveries confirmed out of band, not a conversation loop, so that precedent does not extend to this.

**(a′) Funnel through the structured side channels Boss already has.** This is the honest strongest form of (a): keep the terminal session, send input by paste and confirm it from the transcript, and render output by tailing Claude Code's JSONL transcript (`engine/transcript-tail`, `engine/transcript-markdown`, the app's `TranscriptView`). Its case is real. Nothing about the coordinator changes, every capability is preserved by construction, the fallback is the same live session, and it adds no dependency. Its input path is existing practice: Ideas "Send to Coordinator" already pastes and submits, and the nudge already scans for a blocking dialog first. It would deliver a conversational panel sooner than (b).

It is not chosen because it cannot do what this project's goals ask for, and the reasons are checkable:

- **A dialog can be detected but not rendered or answered structurally.** A permission prompt, an auto-mode fallback question, a login or rate-limit dialog does not appear in the transcript until it is resolved. Boss can detect one on screen (`engine/core/src/coordinator_tmux.rs:937-941` does, before the nudge), but answering it means sending keystrokes into a dialog whose options were read from screen text. The goal that a permission prompt be answerable in the panel would rest on screen parsing, which is option (a).
- **Every richer interaction planned for later is something the terminal UI consumes itself.** The April inventory already said so: "Permission requests must be intercepted, not consumed by TUI."
- **No token streaming.** Transcript records arrive per content block. A long answer appears whole. This is a soft regression from today's pane, not a disqualifier.

(a′) remains the fallback if headless hosting is ruled out for account or policy reasons.

**(c) Drive `claude` directly in stream-json mode.** `claude -p --input-format stream-json --output-format stream-json` gives structured input and output from the same binary and login as today, with no adapter and no Node. The April findings even recommended it as an orchestration channel. It is a serious option and is the first fallback.

It is not chosen first because of who maintains the mapping. The CLI reference documents the flags and the output event types, and offers `--permission-prompt-tool` (an MCP tool Boss would have to serve) for permission prompts, but it does not publish a full specification of the input messages. The complete interface is Anthropic's Agent SDK, which exists for TypeScript and Python only, so a Rust engine would implement it against the CLI directly. The adapter is that mapping, built on the official SDK and released against each SDK version, and it is large: its `src` directory has about eighty files at the top level, with dedicated ones for background tasks, nested subagents, compaction replay, session resume and permission modes. Under (b) Boss codes against a published, versioned protocol with an official Rust crate. The rejection is not "undocumented"; Boss already depends on undocumented Claude Code internals (the transcript format, the inbox socket) and that would disqualify established practice. The cost of (b) relative to (c) is one more process, a runtime dependency, and exposure to the adapter's release pace.

**(d) Call the Messages API and run Boss's own agent loop** (`tools/boss/claude_client`). Rejected. Boss would execute tools itself, which main.md forbids, would bill the API instead of the Claude plan, and would lose Claude Code's memory, subagents, auto mode and settings. `claude_client` is used today for single-shot model calls (pane summaries, the design-doc planner), not agent loops, so its existence is not precedent.

### Where the ACP client lives

**In the app, in Swift.** Rejected. The coordinator supervisor runs "even while the app is closed" (`engine/app/src/app/server.rs:746-750`), the tmux design puts reconciliation in the engine and "no re-adoption logic in Swift", and ACP has no official Swift library (three community ones are listed).

### How the adapter process is hosted

**As a child of the engine over stdio.** Simplest, and what the old PoC did. Rejected as the end state: every engine restart, including every self-update, would end the turn in flight and kill the coordinator's background agents. The tmux design's stated goal is that "the coordinator gets the same process survival as a worker". The spike uses this shape because durability is independent of the protocol question. This rejection has a price, stated under [Other risks](#other-risks): the agent host, the intent log and replay exist only to keep that goal.

**A smart host that terminates ACP itself.** A long-lived daemon would hold the ACP connection and the conversation state, and the engine would be its client. Rejected because the host outlives engine updates: a new engine would have to speak to an old host, so every change to conversation logic would need a compatibility story or a coordinator restart.

**Chosen: a byte relay under tmux.** Described in [Process hosting](#process-hosting). Its surface is small enough to freeze: a record shape, a one-line replay handshake and a single-client rule.

### Which Claude Code binary the adapter runs

The adapter runs, in order: the binary named by `CLAUDE_CODE_EXECUTABLE`, the one shipped beside its standalone executable, or the Agent SDK's bundled one (`src/claude-cli.ts`). Running the adapter's own pinned copy avoids version skew but puts a second Claude Code on the machine, outside the user's updates. **Chosen: point `CLAUDE_CODE_EXECUTABLE` at the installed `claude`.** It keeps the main.md principle literally true, keeps one login and one version, and keeps the update banner meaningful. The cost is skew between a pinned adapter and an auto-updating CLI; the spike measures it and the preflight reports it.

## Chosen approach

### Architecture

```
SwiftUI chat panel            (app: view only)
        │  one request + one topic, Boss vocabulary
engine chat service           (queue, reducer, intent log, handoff delivery)
        │  ACP v1 as JSON lines over a private Unix socket
agent host                    (tmux pane: line relay + wire journal)
        │  stdio
ACP adapter → claude          (Claude Code runs every tool)
```

### Invariants

Each is stated as the property that matters, with the mechanism after it.

- **No restart of the app or the engine ends the coordinator's conversation, cuts off a turn in flight, kills background work the coordinator started, or loses a message the operator has sent.** Mechanism: the agent host and everything under it live in the tmux server, not under the engine; a queued message is written to the engine's intent log before the app is told it was accepted.
- **Boss never executes a tool on the model's behalf.** Mechanism: the engine advertises no `fs` or `terminal` client capability. This is the main.md principle at the level that matters; "runs the unmodified `claude` binary" alone would be satisfied by a client that still intercepted file writes.
- **No Boss RPC lets anything but the app speak as the operator.** Mechanism: the send operation is app-only at the tier gate and again in the handler, independent of the `worker_rpc_tier` flag, following `RecreateCoordinator`. This is an RPC-layer guarantee. It is not a sandbox against a hostile process running as the same user: such a process can type into the coordinator's tmux pane today and could connect to the agent host's socket under this design. The handoff document states the same boundary for the credential.
- **The handoff write credential never crosses the engine-to-app boundary and never enters an audit or diagnostic log.** Mechanism: redaction in the reducer over every text-bearing field of every item, not only engine prompts, because the coordinator repeats the credential in its own `boss handoff write` command. The wire journal and the private input file do hold it; both are engine-owned files with mode `0600` that only the engine reads.
- **The terminal backend and the chat backend are equivalent on process durability, working directory, instructions, allow rules, permission mode, memory, handoff protocol and trust root. They are not equivalent on interface.** A human who attaches a terminal to the chat backend's tmux session sees the host's log, not a conversation. Nothing downstream may assume "it is in tmux, so someone can type into it".
- **The chat journal is what the operator saw. Claude Code's transcript is what the model saw.** They diverge at compaction. Forensics that ask what the model knew use the transcript.

### Process hosting

The coordinator stays a tmux session named `boss-coordinator` on the durable socket, created by `start_new` (`engine/core/src/coordinator_tmux.rs:994`) with the same intent-then-create ordering, spawn token, environment (`BOSS_SPAWN_TOKEN`, `BOSS_SESSION_SCHEMA`, `BOSS_BIN_DIR`, `BOSS_BIN`), `PATH` prefix and `unset ANTHROPIC_API_KEY`. Only the pane command differs. Instead of `claude` behind the terminal relay, it runs `boss-agent-host`, which:

1. spawns the adapter with piped stdio, with `CLAUDE_CODE_EXECUTABLE` set;
2. listens on a Unix socket at a short fixed name in the engine's state root, beside `tmux.sock`, so the path stays well inside the 104-byte limit;
3. forwards lines in both directions between the socket and the adapter;
4. appends every line, in both directions, to a wire journal as `{"seq", "ts_ms", "dir", "line"}`;
5. on each connection, replays from the sequence number the client asks for, then continues live;
6. accepts one client at a time, and lets a new connection replace an older one, because an engine restart leaves a dead older connection behind;
7. writes the adapter's stderr to the pane, so `tmux attach` shows a log;
8. exits with the adapter's status when the adapter exits.

The host is one of Boss's own binaries. It is bundled and signed with the others (`app-macos/BUILD.bazel` `additional_contents`, `installer/BUILD.bazel` `bundled_binaries`, the codesign loop in `installer/release.sh`) and the engine resolves it by absolute path through `engine/worker-bin`, the way it resolves `boss`.

Each coordinator generation gets a directory under the engine's state root, created with mode `0700`, that holds the wire journal, the engine's intent log, a replay snapshot and sent images. Putting these under the state root, not under the session directory, is what keeps two engines from sharing them.

Because the pane dies when the adapter dies, the existing supervisor needs no new detection for that case: `restart_if_dead` (`coordinator_tmux.rs:262`) already treats a missing session or a dead pane as the restart trigger. Because the pane process is the root of everything the coordinator runs, the handoff trust check (the caller must descend from the coordinator pane pid, `coordinator_peer` at `engine/app/src/app/coordinator_handoff.rs:29-32`) holds unchanged. The engine verifies the socket's peer pid against the pane pid before using it, mirroring the checks the inbox client makes today.

The spike found a second failure the supervisor cannot see (S16). When Claude Code dies under a live adapter, the adapter stays up: the pane is alive, the prompt in flight fails with JSON-RPC error `-32603`, a pending permission request is withdrawn with `$/cancel_request`, and every later prompt fails with `-32002`. The adapter does not restart the CLI. So the engine detects this case from those errors, and, as already planned under [Session lifecycle](#session-lifecycle), kills the session by token so the supervisor recreates it. The spike also showed that `session/resume` with the same session id starts a new CLI on the same adapter and the conversation continues with its history. Using that instead of a recreate would keep the conversation; the lifecycle entry decides, because it changes what a crash costs.

The session directory must not be inside Claude Code's own temporary root (`/private/tmp/claude-<uid>/`): from there Claude Code does not load the directory's `CLAUDE.md` (S2). The production directory under Application Support is unaffected. An isolated engine's session directory sits beside its socket, so the rule there is about where the socket is put.

The record shape, the replay handshake and the single-client rule are the host's whole surface. A host survives engine updates, so changing any of them means the engine must keep speaking the old form for as long as a host of that version can still be alive, which is one coordinator generation.

### ACP session

On a fresh host the engine sends `initialize` with protocol version 1, no `fs`, no `terminal`, and the adapter's extension capabilities `backgroundSubagents` and `asyncTasks` under `clientCapabilities._meta.jetbrains.air` (version 1). It then sends `session/new` with the session directory as `cwd`, no MCP servers, and `_meta.claudeCode.options` carrying `settingSources` (`user`, `project`, `local`, so that `CLAUDE.md` and `.claude/settings.local.json` load) and `settings` (the inline settings the terminal launch passes with `--settings`). It sets the model with `session/set_config_option` (`configId` `model`), then selects the `auto` session mode with `session/set_mode`.

These names were pinned by the spike. Two of them differ from what this design first assumed:

- **The model is a session option, not a creation option.** A model passed in `_meta.claudeCode.options` is overridden by the user's own configured model (S3).
- **The extension capabilities are required, not optional.** Without them the adapter holds a prompt open while a background agent runs and gives no boundary for a turn the agent starts itself (S6). They are an adapter extension, so the minimum adapter version the engine accepts is the one these names were observed in, and the preflight checks that the adapter's `initialize` response lists both.

The adapter starts Claude Code with `AskUserQuestion` disallowed, so a chat-backed coordinator cannot ask a structured question; it asks in prose. It also passes `--allow-dangerously-skip-permissions`, which is why the session offers a `bypassPermissions` mode. The engine never selects that mode. The adapter documents that setting `allowDangerouslySkipPermissions` to `false` in the creation options removes it from the session; the spike did not test that, and the lifecycle entry does.

After an engine restart the engine does not initialise again. It reconnects to the host socket and rebuilds its state by replay, described below.

The engine uses the official crate for the typed schema and owns the line framing and request-id table itself, because replay needs to recognise a response to a request an earlier engine process sent. Spike criterion S15 settled this: the crate's own connection type runs on an already-initialised stream and exposes its request ids, but it drops the response to a request it did not send and offers no way to register one. The client crate `tools/boss/acp_client` has the framing, the journal record and a connection whose `expect_response` registers an id read from the journal.

The agent numbers its own requests from 0, independently of the engine's ids, so a response is matched by direction as well as by id.

### Turn queue and intent log

The engine holds one FIFO of prompts for the session. Each has an origin: `operator` (from the app), `engine` (brief, nudge, reset request, authorization), or none for a turn the agent starts itself. ACP v1 says only that "once a prompt turn completes, the Client may send another", so the engine sends one `session/prompt` at a time. A message typed during a turn is queued and shown as queued. Engine prompts go ahead of queued operator prompts and never pre-empt a turn in flight.

The wire journal cannot say why a prompt was sent: an operator prompt and an engine prompt are the same `session/prompt` line, and a queued prompt is not on the wire at all. So the engine keeps its own append-only **intent log** in the generation directory. It records each prompt when it is queued (origin, the app's message id, the text, any image reference), when it is sent (which request id carried it) and when it is withdrawn. A message is written there before the app is told it was accepted.

A turn the agent starts itself is opened and closed by the adapter's task extension, not by the arrival of updates. `async_task_spawned` with `taskType: "turn"` opens it and `async_task_state_update` with the same `asyncTaskId` and a state of `completed`, `failed` or `stopped` closes it (S6). That is how a background agent's completion and a peer session's message both appear. The queue sends no prompt while such a turn is open. Updates that arrive with no prompt in flight and no such task open do not open a turn: a background agent's own tool calls arrive that way, and so do session metadata updates. What started a turn (`human`, `peer` or `task-notification`) is reported in the last usage update of the turn, after its end.

The adapter queues a second `session/prompt` sent during a turn and answers both in order (S9). The engine still sends one at a time: a prompt the adapter has queued can no longer be returned to the composer.

Stop sends `session/cancel`; per the protocol, the engine answers every pending permission request with `cancelled` first. Stop cancels only the turn in flight. Operator messages still queued are returned to the composer as a draft and not sent, because the operator pressed Stop when something was going wrong. Queued engine prompts proceed.

### Conversation reducer and replay

The reducer is a pure function from the wire journal and the intent log, joined by request id, to connection state plus an ordered list of chat items. It runs the same way on replay and live. Item ids derive from journal sequence numbers, so a fetch by sequence is stable across engine restarts. The two logs are the source of truth; the engine persists nothing else about the conversation.

Replaying a long-lived generation from its first record would grow without bound, so the engine periodically writes a versioned snapshot of the reducer's state with the sequence number it covers. A restart loads the snapshot and replays the tail. The snapshot is a cache: if it is missing, unreadable or from another version, the engine replays from the start.

The reducer reads three things the official typed schema does not give it. The task updates above fail to parse as a typed `SessionNotification` and are read from the raw line. `$/cancel_request` from the agent withdraws one of its own requests, which resolves a pending permission item without an answer (S16). The adapter's account notifications carry the signed-in identity and are reduced to a state, never to their content.

Redaction happens here. The reducer is given the generation's write credential and removes it from every text field of every item and from the message in progress. For streamed text it works on the accumulated buffer and holds back a tail one character shorter than the secret, so a credential split across two chunks cannot leak in part. Substring redaction of this credential already exists for audit events (`engine/app/src/app/coordinator_handoff.rs:481`).

Streaming text is not stored per chunk in the projection. Completed message blocks become items; the message in progress is held as a snapshot and pushed as deltas carrying their offset, so the app can detect a gap and re-fetch. This matters because the session's outbound queue can drop pushes under load (`ResyncRequired` in `protocol/src/wire/topic_event_payload.rs`).

The live generation's logs are never pruned. Older generations are pruned by an age and size policy. Following the rule in [BOSS_COORDINATOR.md](../../../../BOSS_COORDINATOR.md) for limits, that policy, the snapshot interval, the delta publish interval, the tool-output truncation size and the image cap are set from measurements taken in the entries that introduce them, not chosen here. Existing timeouts (`DELIVERY_TIMEOUT`, `HANDOFF_TIMEOUT`, `LOCK_WAIT_TIMEOUT`, `PENDING_COALESCE_WINDOW`, `AUTHORIZATION_CONFIRMATION_TIMEOUT`) are reused unchanged.

### Where the code lives

`coordinator_tmux.rs` belongs to the `boss-engine-runtime` crate (`engine/core/runtime/Cargo.toml`), and `engine/app` depends on it, not the reverse. So the chat service cannot live in `engine/app`. The reducer, queue, intent log and the service that holds the host connection all live in a new crate, `engine/coordinator-chat`. The runtime crate depends on it for launch, adoption and handoff delivery. `engine/app` holds only the RPC handler and owns the service handle, which it passes to the lifecycle code the way `CoordinatorSpawn` (`coordinator_tmux.rs:82`) already carries the lifecycle's other dependencies.

### Engine-to-app protocol

A new request or event costs about a dozen files in this codebase, most of them one-line edits in exhaustive matches ([cut-per-change-file-fan-out](cut-per-change-file-fan-out-in-the-boss-codebase.md)). So the surface is one of each, with the variety inside:

- **One request**, `CoordinatorChat`, with an operation: send (text, attachments, a client message id), cancel, answer permission, fetch (after a sequence number).
- **One topic**, `coordinator.chat`, with one payload carrying the coordinator generation and either an item, a delta or a state change. State changes include "the chat backend could not start", with the reason.

Chat items are Boss's vocabulary: message (operator, coordinator, engine or peer), thought, tool call (title, kind, status, bounded input and output, parent for nesting), permission request (options and resolution), turn boundary (origin, stop reason), notice (session started, compaction, error, login required), and usage. An unknown item kind renders as a collapsed generic row, so a later phase can add a card without a protocol change.

The engine tells the app which backend is live by adding a field to the existing `AttachCoordinatorPaneInput` (`protocol/src/engine_app.rs:92`).

### Chat panel

The panel replaces `BossPaneTerminalView` inside `workBossPanel` (`app-macos/Sources/ContentView.swift:782`) when the chat backend is live. The header, the update banner and the reset status inset are unchanged. The workaround that keeps the terminal mounted at full width while the panel is collapsed is not needed.

- A new `CoordinatorChatModel` owns chat state, bound like `RemotePaneViewer`, so `ChatViewModel` does not grow.
- The list is lazy, follows the tail with a "jump to latest" control (the pattern in `TranscriptView`), and renders completed blocks with `StructuredText(markdown:).bossMarkdown()`. The streaming tail is re-parsed at a bounded rate.
- The composer is a multi-line text view: Return sends, Shift-Return inserts a newline, Escape stops the turn.
- A status row shows idle, working, queued or waiting for permission, and context usage when the adapter reports it.
- Failures appear inline in the panel. No modal dialogs, per `app-macos/AGENTS.md`. When the chat backend cannot start, the panel shows the reason and one action, "Switch to terminal coordinator".

### Capabilities: preserved or changed

Where a "Today" cell cites no Boss source, it is because Boss has no code for it: the behaviour belongs to the Claude Code terminal UI.

#### Session lifecycle

| Capability      | Today                                                                                                                                                  | Chat backend                                                                                                                                                                                                                                                                                                                                                                                        |
| --------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Launch          | App registration calls `ensure_for_attach` then `start_new` (`coordinator_tmux.rs:246`, `:994`). Argv is built in `coordinator_tmux/inbox.rs:101-108`. | Same entry points. `start_new` picks the pane command by backend. A preflight checks the agent host, the adapter and `claude` first.                                                                                                                                                                                                                                                                |
| Restart         | Supervisor loop, first pass at 500 ms then every 10 s, calls `restart_if_dead` (`engine/app/src/app/server.rs:746-873`).                               | Unchanged. The host exits with the adapter, so a dead pane still means a dead coordinator.                                                                                                                                                                                                                                                                                                          |
| Reset           | `RecreateCoordinator` → reset handoff → `recreate_after_confirmation` (`coordinator_tmux.rs:493`).                                                     | Unchanged flow. Also how the backend changes: the next reset starts the configured backend.                                                                                                                                                                                                                                                                                                         |
| Crash recovery  | A dead pane or missing session starts a fresh session with a brief.                                                                                    | Same. New case: the adapter is alive but reports the session failed. The engine kills the session by token and the supervisor recreates it. S16 showed the case: the prompt in flight fails with error `-32603`, later prompts with `-32002`, and the pane stays alive, so the engine detects it from those errors. `session/resume` on the same adapter also revives the session with its history. |
| Engine restart  | `reconcile_existing` adopts by exact token match (`:559`).                                                                                             | Same match, then reconnect and replay. If the socket cannot be reached, recreate visibly.                                                                                                                                                                                                                                                                                                           |
| Restart ceiling | `LIMIT = 5`, `BACKOFF_CAP = 60 s`, two counters, one attention each (`:415-416`; `server/startup_attentions.rs:100`, `:121`).                          | Unchanged counters, attentions and behaviour at the ceiling. A failed preflight, `initialize` or `session/new`, or a login-required error, counts as a failed start.                                                                                                                                                                                                                                |
| What persists   | The `coordinator.tmux_*` record, the handoff, the prompt hash. No session id, no resume.                                                               | The same, plus `coordinator.backend` in `metadata` and the generation directory. Still no resume.                                                                                                                                                                                                                                                                                                   |

The brief is still composed in `start_new` and still written to the private input home, as today. Once the session exists the engine queues it from there, which records it in the intent log. An engine that restarts and finds a session with no brief in its intent log queues it then. That replaces the relay's marker files without a second persisted flag.

#### Handoff

| Capability                       | Today                                                                                                                                         | Chat backend                                                                                                                                      |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| Brief as first prompt            | Composed by `prepare_session_start_brief` (`coordinator_tmux.rs:1139`), pasted by the terminal relay (`engine/driver/src/initial_prompt.rs`). | Same text, sent as the first `session/prompt`. It still never appears in argv or environment. The archive at `.claude/handoff-brief.txt` is kept. |
| Private write authorization      | Appended to the brief (`:1229`); redelivered through the inbox on adoption or `boss handoff show --redeliver`.                                | Same rules. Redelivery is an engine prompt through the queue. The brief's one phrase naming the inbox is reworded.                                |
| Reset request                    | `[Boss reset request id=…]` through the inbox (`coordinator_tmux/reset_handoff.rs:96`), 15 s delivery bound, 30 s grace.                      | Same text and bounds. Turn end comes from the prompt response, not from reading the transcript.                                                   |
| `boss handoff verify/write/show` | CLI to engine RPC; ancestry plus credential (`cli/src/handoff.rs`, `engine/app/src/app/coordinator_handoff.rs`).                              | Unchanged. The coordinator still runs these as shell commands.                                                                                    |
| Failure text                     | Begins `coordinator did not receive the handoff request`; the app keys on that prefix.                                                        | Same prefix. The named stage changes: "the coordinator is mid-turn" becomes a fact the engine knows, not an inference.                            |

The delivery seam already exists: `HandoffDelivery` (`coordinator_tmux/inbox.rs:27`). The chat backend is a second implementation, and its methods map onto the queue like this:

- `deliver` (the reset request) resolves when the request's `session/prompt` has been sent. If a turn is in flight it waits in the queue, bounded by the existing `DELIVERY_TIMEOUT`. On timeout the entry is withdrawn from the queue, so it cannot be delivered late. Today a late delivery is only neutralised by the verify step failing.
- `deliver_accepted` (the authorization) reports "accepted" once the entry is in the intent log, and "confirmed" once it has been sent. The authorization id registers at acceptance, as it does at inbox acceptance today. A request the coordinator makes mid-turn is therefore always accepted at once and confirmed when that turn ends, as today. The wait for confirmation keeps `AUTHORIZATION_CONFIRMATION_TIMEOUT`, and repeat requests coalesce under `PENDING_COALESCE_WINDOW`.
- `turn_ended` is the response to that prompt.

Pending reset ids live in engine memory today (`reset_handoff.rs`), so a reset request that outlives an engine restart already fails verification. The entry that implements this confirms the same holds for authorization ids, and withdraws on restart any intent-log entry whose id the new engine no longer holds.

The verify step is kept even though the engine's channel is now private, so that the same prompt serves both backends during the transition and inbox messages, if they still exist, stay untrusted.

#### Messages into the session

| Source                                  | Today                                                                                                                        | Chat backend                                                                                                                                                                                                                                                                                                                  |
| --------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The operator                            | Keystrokes through a tmux client in Ghostty (`BossPaneModel.swift:67`).                                                      | The send operation; verbatim text as an ACP prompt.                                                                                                                                                                                                                                                                           |
| Ideas "Send to Coordinator"             | Pastes into the Ghostty surface and presses Return (`ContentView.swift:189-193`).                                            | The same send operation. This also gives it a delivery result, which it lacks today.                                                                                                                                                                                                                                          |
| Engine: brief, reset, authorization     | Terminal relay and inbox.                                                                                                    | Engine prompts through the queue, shown as engine messages with the credential redacted.                                                                                                                                                                                                                                      |
| Engine: prompt-changed nudge            | `send_keys`, no delivery proof (`maybe_nudge_prompt_change`, `coordinator_tmux.rs:872`).                                     | An engine prompt. Gains delivery proof.                                                                                                                                                                                                                                                                                       |
| Background agent completion             | Internal to Claude Code: it re-invokes the session. Boss only tells the coordinator to use them (`BossPaneModel.swift:543`). | Still internal. With the adapter's task extension it reaches the engine as an agent-started turn with an explicit start and end (S6), which the panel shows. The background agent's own tool calls arrive outside any turn.                                                                                                   |
| Other Claude Code sessions (peer inbox) | Accepted without approval because of the launch setting (`inbox.rs:97`).                                                     | Undecided; see [Messages from other Claude sessions](#messages-from-other-claude-sessions). Plan of record: keep the setting, and show any that arrive as peer, not operator. The lifecycle entry applies whichever answer is recorded. S10 confirmed the inbox exists under the adapter and that a peer message is answered. |

#### Instructions

| Item                  | Today                                                                                                                             | Chat backend                                                                                    |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| System prompt         | `bossSystemPrompt` (`BossPaneModel.swift:399`, about 52 KB) written to `boss-session/CLAUDE.md`; loaded as project memory.        | Same file, same directory. Not moved to ACP's `systemPrompt`, so both backends read one source. |
| `BOSS_COORDINATOR.md` | Read from GitHub in `start_new`, rendered into the brief (`engine/core/src/coordinator_guidance.rs:94`, `:369`).                  | Unchanged; it is part of the brief. `boss guidance show` unchanged.                             |
| Memory directory      | Claude Code's own per-project memory, keyed by working directory. Boss configures nothing; the prompt governs its use (`:466`).   | Same working directory, so the same store. S2 confirmed the adapter-hosted CLI reads it.        |
| Skills                | None configured by Boss; user-level ones load.                                                                                    | Same, through the user setting source.                                                          |
| Settings              | `.claude/settings.local.json` with `permissions.allow` and `autoMode.allow` (`BossPaneModel.swift:176-294`); inline `--settings`. | Same file through the local setting source; the inline settings through session options.        |

Tests that pin the prompt's location (`cli/src/handoff.rs:265`, `:302` include the prompt source and assert on `boss handoff verify`) keep passing because the prompt does not move.

#### Tool use and permissions

| Item                           | Today                                                                                                                            | Chat backend                                                                                                                                                                       |
| ------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Mode                           | `--permission-mode auto` (`inbox.rs:103`).                                                                                       | The `auto` session mode, which the adapter lists (`src/session-mode.ts`).                                                                                                          |
| Classifier                     | Runs inside Claude Code; allow rules pre-clear `boss` commands (`BossPaneModel.swift:198-237`).                                  | Still inside Claude Code. A denial appears as a failed tool call and is labelled as blocked by auto mode.                                                                          |
| Permission prompts             | A dialog in the terminal. Nothing in Boss can answer it; the allow rules exist so it "must never stall" unattended (`:218-223`). | `session/request_permission` becomes a card with the agent's options. With no app connected it still waits, but the engine now knows and the panel says so on reconnect.           |
| Tool calls                     | Rendered by the terminal UI.                                                                                                     | One generic row per call: title, kind, status, expandable bounded input and output. A subagent call is one row with its result; nested transcripts are not requested.              |
| Streaming                      | The terminal streams tokens.                                                                                                     | `agent_message_chunk` deltas. Thinking is collapsed.                                                                                                                               |
| Interrupt                      | Escape typed by the human. The engine has no coordinator interrupt.                                                              | Stop or Escape sends `session/cancel`. Background agents are expected to keep running, as today; the spike confirms.                                                               |
| Long-running background agents | Live in the `claude` process in tmux; survive engine and app restarts.                                                           | Live in the `claude` process under the agent host; survive the same restarts. The session's input stays open, so the documented exit-time wait cap should not apply; S6 covers it. |

#### Transcript

| Item                                 | Today                                                                                                                         | Chat backend                                                                                                                             |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Persistence                          | Claude Code's JSONL under `~/.claude/projects/<encoded cwd>/` (`engine/core/src/coordinator_handoff.rs:345`).                 | Still written by the CLI, expected in the same directory. The brief's "last resort" pointer stays valid. The chat journal is additional. |
| Scrollback                           | tmux history, 2,000 lines (`tmux/src/lib.rs:136`), lost on recreate.                                                          | The logs for the current generation, fetched in pages. Not bounded by a terminal buffer.                                                 |
| Compaction                           | Not detected. The prompt tells the coordinator to run `boss handoff show --redeliver` afterwards (`BossPaneModel.swift:472`). | The adapter reports it; the panel shows a divider and keeps earlier items. The redeliver instruction still works through the queue.      |
| `bossctl agents transcript`, `focus` | Worker runs only (`bossctl/src/agents.rs:1751`). Never worked for the coordinator.                                            | No change.                                                                                                                               |

#### Rendering

- **Markdown and code blocks.** Textual with the existing Boss theme: headings, lists, tables, block quotes and highlighted code blocks all exist. No renderer work is needed beyond the streaming tail.
- **Pasted images.** The composer accepts image paste and file drop, shows chips, and sends images as ACP `image` blocks (base64 with a MIME type; needs the agent's `image` prompt capability, and the adapter's README lists images as supported). The app hands the engine a file, not a base64 frame. The engine validates it, stores it in the generation directory, and records a reference in the intent log so the sent message shows a thumbnail after a restart. The existing attachment store is not reused: its rows require a work item and an execution (`engine/work-db/src/schema_baseline.rs`), so holding a chat image there would need a schema change, which is a non-goal. What is reused is its format sniffing (`engine/attachments/src/image.rs`) and, in the app, `AttachmentImageCache`, which gains an entry point keyed by file path. Images are pruned with their generation.
- **Images in answers.** Not rendered in phase one. Textual has no image loader configured. A local path in an answer stays a link.
- **Work-item links.** `T<n>`- and `P<n>`-shaped tokens in completed coordinator text become links. A link carries only the id as written. Clicking it sends the engine the same `RevealWorkItem` request that `bossctl reveal` sends today (`bossctl/src/agents.rs:1136`), so the panel inherits that verb's rule for which product an id belongs to and does not invent a second one. Code spans and code blocks are not linkified.

#### Model and driver

The model stays configurable (`DEFAULT_COORDINATOR_MODEL`, `BOSS_COORDINATOR_MODEL`, `engine/core/src/config.rs:119`), passed at session creation. The model-mismatch recreate flow is unchanged.

The driver stays Claude Code. Today it is named in code, not selected: the executable is the literal `"claude"`, and the lifecycle asks the driver registry for `"claude"` by name and passes `ClaudeDriver` directly. ACP removes one of the ties, the wire protocol: other drivers already speak it (Grok's session log is an ACP update stream; the Antigravity CLI has an `acp` subcommand; a Codex adapter, `@agentclientprotocol/codex-acp`, is listed but was not examined here). It does not remove the others. Instruction delivery through `CLAUDE.md`, the allow rules, auto mode, the memory store and the handoff's trust model are all Claude Code features. A second coordinator driver would be its own project; this design neither blocks it nor claims it.

### Terminal affordances and their disposition

| Affordance of the Claude Code terminal UI                     | Phase one                                                                                                                                   |
| ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| Typing, multi-line input, queued input while working          | Kept, natively.                                                                                                                             |
| Streaming answer, visible tool activity, collapsed thinking   | Kept, natively.                                                                                                                             |
| Escape to interrupt                                           | Kept (`session/cancel`).                                                                                                                    |
| Image paste                                                   | Kept, natively.                                                                                                                             |
| Slash commands typed as text (for example `/compact`)         | Passed through as a prompt. `/compact` and `/context` work; `/help` and `/status` are answered "isn't available in this environment" (S12). |
| Pickers and dialogs (`/model`, `/mcp`, `/memory`, `/login`)   | Dropped. Model changes use Boss's setting; login uses `claude` in a terminal.                                                               |
| Plan mode, question dialogs                                   | Not built. If the adapter surfaces them as permission requests they render as cards.                                                        |
| Context meter                                                 | Shown in the status row from the adapter's usage updates if present.                                                                        |
| `/clear`                                                      | Not supported. Reset Coordinator is the way to start fresh.                                                                                 |
| Shell shortcuts, history search, vim keys, verbose transcript | Dropped.                                                                                                                                    |

### Parity list

The chat backend is at parity when each of these holds on the real system, with the real adapter and the real login:

1. A typed message gets a streamed answer, and tool calls and their results are visible.
2. The coordinator behaves as the coordinator: its instructions, allow rules and memory are in effect, and `boss` and `bossctl` commands run without prompting.
3. Auto mode is in force; a permission request can be answered from the panel.
4. Stop interrupts a turn and the next message works.
5. A background agent's completion arrives without any prompt and its result is not lost.
6. A pasted screenshot is seen by the model.
7. The session-start brief arrives on a fresh session and states one of the five handoff states.
8. Reset writes and carries a handoff; a forced reset works; authorization redelivery works.
9. An engine restart mid-turn and an app restart mid-turn each lose nothing, including a message that was queued at the time.
10. A killed adapter is replaced within the supervisor's cadence, and five failures raise the existing attention.
11. Switching to the terminal coordinator, from the flag or from the panel's failure state, returns a working coordinator with the handoff intact.

### The spike: a validating study

**What kind of study.** It validates (b). It can return "ACP through the adapter works for the coordinator" or "it does not, for this reason". It cannot show that (a′) or (c) would be better, and its results must not be read as that comparison.

**What is genuine and what is not.** The adapter, the `claude` binary, the login, the model, the rendered coordinator prompt and settings are all real. The engine service, the agent host and handoff delivery are not in the spike, so nothing about them is validated by it; each later entry carries its own acceptance, and the parity run at the end exercises the whole path.

**Setup.** A probe binary in the new client crate connects to the adapter as a child process, in a temporary copy of the session directory that the app's own code renders, with `boss` pointed at an isolated engine started by the recipe in `.claude/CLAUDE.md`. The spike installs the pinned adapter into a temporary directory; it does not add it to the build. The probe records every line in both directions in the wire journal's shape. A fixture in the app, reachable only in an isolated instance, launches the probe by path and renders the conversation; it does not link the crate. The spike never touches the production session directory or socket. Recordings are not checked in: they hold the prompt and local paths. The findings quote short excerpts. If the environment cannot run a real Claude session, the spike stops and reports that; a fake agent is not a substitute.

**Criteria.** Each is observed and recorded with the journal excerpt that shows it.

| Id  | Observation                                                                                                                                                                                                      | Pass means                           |
| --- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------ |
| S1  | `initialize` and `session/new` succeed; a three-turn conversation streams. Adapter, SDK and CLI versions recorded.                                                                                               | Required.                            |
| S2  | The rendered prompt and the allow rules are in effect: a pre-cleared `boss` command runs without a request. A marker placed in the memory directory that Claude Code derives from the working directory is read. | Required.                            |
| S3  | The session runs in `auto` with the coordinator's model. If a classifier denial can be provoked safely, its structured shape is recorded.                                                                        | Required, except the denial.         |
| S4  | A permission request is answered allow and reject from the fixture.                                                                                                                                              | Required.                            |
| S5  | `session/cancel` ends a turn in flight and the next prompt works.                                                                                                                                                | Required.                            |
| S6  | A background agent that finishes after its turn ended produces updates and an answer with no prompt in flight, with a start and an end a client can rely on.                                                     | Required, including the end signal.  |
| S7  | A pasted PNG is described by the model.                                                                                                                                                                          | Required.                            |
| S8  | The session runs on the Claude plan login with `ANTHROPIC_API_KEY` unset.                                                                                                                                        | Required.                            |
| S9  | A second `session/prompt` sent mid-turn: queued, steered or rejected.                                                                                                                                            | Recorded; shapes the queue.          |
| S10 | Whether the hosted CLI registers a cross-session inbox, and how a peer message appears.                                                                                                                          | Recorded; shapes peer handling.      |
| S11 | Claude Code's transcript appears under the expected directory in a dialect `engine/transcript-markdown` parses.                                                                                                  | Recorded.                            |
| S12 | `/compact` and two other slash commands typed as prompts: what is emitted.                                                                                                                                       | Recorded.                            |
| S13 | The installed `claude` under `CLAUDE_CODE_EXECUTABLE` versus the adapter's own copy: any startup failure.                                                                                                        | Recorded; shapes the preflight.      |
| S14 | Render cost per delta in the fixture for a long answer with code blocks.                                                                                                                                         | Recorded; sets the publish interval. |
| S15 | Whether the official crate's connection type can run over an already-initialised stream and expose request ids.                                                                                                  | Recorded; decides who owns framing.  |
| S16 | The `claude` child is killed under a live adapter: what the adapter does.                                                                                                                                        | Recorded; shapes crash recovery.     |

S2 shows that memory is read from the directory derived from the working directory. That the production coordinator's existing memory carries over then follows from the chat backend using the same working directory; it is confirmed on a real session by parity item 2.

**How the result gates later entries.**

- All required criteria pass: proceed. The findings amend this document in the same change, and the recorded criteria fill in the details the later entries reference.
- A required criterion fails: repeat that one behaviour against `claude` in stream-json mode with no adapter, to tell which side failed. This is a diagnostic for choosing the branch below, not a comparison of the options.
- The behaviour works without the adapter: switch to (c). The agent host, the journal, the queue, the RPC surface and the app entries keep their shape. The client crate, the reducer's mapping and a permission-prompt tool are re-planned in a revision of this document before those entries start.
- The behaviour fails without the adapter too, or S8 fails, or the account question below is answered no: stop. The remaining entries are withdrawn and the project is re-planned around (a′) in a revision of this document. Nothing is quietly adapted.

### Spike result

Run on 2026-11-25. The full record, with the wire lines for each criterion, is in [coordinator-acp-spike.md](../investigations/coordinator-acp-spike.md).

**The gate passes: all eight required criteria were observed passing.** The first branch above applies: proceed. Three passes carry a condition that later entries now state, and S3's optional classifier denial was not obtained.

| Id  | Result                                                                                                                                                 |
| --- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| S1  | Pass. Three turns streamed; adapter 0.110.0, Claude Code 2.1.332, crate 5.5.1.                                                                         |
| S2  | Pass, when the session directory is outside Claude Code's temporary root.                                                                              |
| S3  | Pass, when the model is set with `session/set_config_option`. No classifier denial was provoked by three harmless commands.                            |
| S4  | Pass. Allow and reject answered from the fixture.                                                                                                      |
| S5  | Pass. Cancel ended a streaming turn and a turn waiting on a permission request.                                                                        |
| S6  | Pass with the `backgroundSubagents` and `asyncTasks` extension capabilities; start and end are task updates. Without them the prompt is held open.     |
| S7  | Pass. A pasted PNG was described.                                                                                                                      |
| S8  | Pass. Plan login, no API key.                                                                                                                          |
| S9  | Queued: both prompts answered in order.                                                                                                                |
| S10 | The inbox is registered; a peer message is answered as an agent-started turn.                                                                          |
| S11 | The transcript is where the engine expects it and parses.                                                                                              |
| S12 | `/compact` and `/context` work; `/help` and `/status` are refused. Compaction arrives as a tool call.                                                  |
| S13 | No startup failure with the installed CLI or the adapter's own.                                                                                        |
| S14 | About 20 deltas a second; 17.6 ms a delta to re-parse and lay out, growing with length. Publish at 100 to 150 ms and re-parse only the streaming tail. |
| S15 | The crate's connection cannot recover an earlier process's request. Boss owns framing and the id table.                                                |
| S16 | The adapter survives a dead Claude Code; the session stays dead until it is recreated or resumed.                                                      |

What the result changes in later entries:

- **Durable agent host.** Unchanged. The adapter exits with status 0 when its input closes, which only happens when the pipe handle is dropped, not when it is shut down.
- **Chat item vocabulary and conversation reducer.** The reducer reads the task updates and `$/cancel_request` from raw lines, takes a turn's origin from its last usage update, and treats compaction as the tool call the adapter sends. Tool output arrives as fenced markdown text. No thought chunks were seen on the wire, so the thought item may stay empty. Its recorded fixtures include an agent-started turn.
- **Turn queue, intent log and replay snapshot.** Agent-started turns open and close on the task updates. Replay sizing can use the measured line sizes: the largest line recorded was 27,587 bytes.
- **Coordinator lifecycle on the chat backend.** The session setup is the sequence under [ACP session](#acp-session). The preflight checks the extension capabilities. A dead Claude Code under a live adapter is detected from the prompt error; whether to recreate the session, as planned, or to keep the conversation with `session/resume` is decided there.
- **Chat panel: conversation rendering.** The publish interval and the split between completed blocks and the streaming tail are set from S14.
- **Chat panel: composer, send, queue and stop.** `/help` and `/status` typed into the composer are refused by the adapter in one line; nothing special is needed.
- **Messages from other Claude sessions.** A peer message is answered whether or not the inbound setting is passed on this machine, and the sender's name and text are available only after the answer. The decision under [Messages from other Claude sessions](#messages-from-other-claude-sessions) stands; labelling such a turn as peer happens when its origin arrives.

### Switch-over and fallback

- **Flag.** `coordinator_native_chat` in the engine's feature-flag registry (`engine/feature-flags/src/lib.rs`), default off, following `editorial_controls`. It selects the backend for the next coordinator start; it does not kill a running coordinator. Settings shows that it applies at the next reset.
- **Stages.** While entries land, the flag is for isolated engines only: between the lifecycle entry and the permission entry, a coordinator on the chat backend cannot be typed to, stopped, or given a permission answer, and the flag's description says so. After the permission entry the repo owner can opt in. After the parity run, a soak whose length the repo owner sets. Then the default flips. The terminal backend stays selectable until it is retired.
- **Fallback.** Turn the flag off, then Reset Coordinator. When the chat backend cannot start, the panel offers both steps as one action, beside the reason. The handoff carries context across, in either direction, because both backends share the session directory, prompt, settings, memory and handoff store.
- **Why the fallback is not automatic.** The tmux design forbids "an _automatic_ degradation the system chooses on its own" and allows an operator-set, visible mode. A chat backend that fails its preflight or reaches the restart ceiling therefore behaves as the terminal coordinator does at its ceiling today: it stays down and raises the existing attention, with the reason. The operator's one click is the decision.
- **Retirement.** Removing the terminal coordinator code is deferred until after the soak.

### Premises this design changes

These are stated elsewhere as fact and stop being true. Each is changed in the entry that makes it false, in the same diff.

- [main.md](main.md): "A general ACP / SDK client. V2 is terminal-embed only" (non-goal) and "ACP / SDK-based workers" (out of scope). The lifecycle entry amends these to say the coordinator alone has a structured client and that tool execution stays with Claude Code.
- [coordinator-session-handoff.md](../coordinator-session-handoff.md): delivery "through Claude Code's local inbox" and the brief "through the terminal relay". The handoff entry adds the chat backend's transport beside them.
- [test-instance-isolation.md](test-instance-isolation.md): whatever it says about the coordinator session is updated by the isolation entry.
- `//tools/boss/engine/core:tmux_only_surface_test` scans Swift sources for identifiers such as `sendToPane`. The new send path must not reintroduce them; the test is not relaxed.
- `coordinator_inbox_live_test` and `coordinator_inbox_real_model_test` pin inbox delivery. They stay for the terminal backend. The chat backend gets equivalent `manual` targets.

### External sources

- ACP protocol: <https://agentclientprotocol.com/protocol/overview>, with the initialization, session-setup, prompt-turn, content, tool-calls and transports pages beside it. Stable version 1; version 2 entered Preview on 2026-11-20. One defined transport: newline-delimited JSON-RPC over stdio. This design targets version 1.
- Rust crate: `agent-client-protocol` 5.5.1, Apache-2.0.
- Adapter: <https://github.com/agentclientprotocol/claude-agent-acp>, package `@agentclientprotocol/claude-agent-acp` 0.110.0 (released 2026-11-23), Apache-2.0, built on `@anthropic-ai/claude-agent-sdk` 0.3.330. Files read: `src/claude-cli.ts`, `src/new-session-meta.ts`, `src/client-system-prompt.ts`, `src/session-mode.ts`, `src/auth-methods.ts`, `src/async-tasks.ts`, `src/task-notifications.ts`.
- Anthropic: <https://code.claude.com/docs/en/headless>, <https://code.claude.com/docs/en/cli-reference>, <https://code.claude.com/docs/en/legal-and-compliance>, <https://support.claude.com/en/articles/15036540>.

## Risks / open questions

### Usage metering and account policy

Options (b) and (c) both run the coordinator through Claude Code's headless path. Anthropic's help centre says today: "when you're signed in with your Claude plan, the Claude Agent SDK and `claude -p` still draw from your subscription limits." The same page records that a change to this was announced and then paused on 2026-06-15, and that plan API credits added on 2026-10-07 apply only with a Console API key. The compliance page says plan limits "assume ordinary, individual usage of Claude Code and the Agent SDK" and that products should not offer Claude.ai login to their own users. Boss does not handle credentials; the user signs in through `claude`.

The risk is that the coordinator, a long-lived and heavily used session, moves to a different usage pool or policy class than interactive use, now or after a future change. The April decision record settled an outside policy question first, before any engineering comparison, because a "no" would have ended the option. That question was whether custom clients were permitted, and its answer does not cover this one. Plan metering is a new question of the same kind. **The repo owner should decide whether this is acceptable before the spike starts.** If not, (b) and (c) are both out.

### Adapter provisioning

The adapter is a Node package or a standalone executable. The plan of record treats it like `claude` and `tmux`: installed on the machine, located by an engine setting, checked by a preflight, with a clear inline state when it is missing. That is cheapest and enough for the flag period. For other people installing Boss, a missing adapter would mean a coordinator that does not start until they switch to the terminal one. Bundling a pinned copy is a deferred entry; if it is chosen it lands before the default flips. The spike reports the archive size so the choice can be made with numbers.

### Messages from other Claude sessions

Whether any same-user Claude Code session should be able to put text in front of the coordinator was never decided; it is a side effect of how Boss delivered its own messages. Under the chat backend Boss no longer needs the inbox. The plan of record keeps today's behaviour where the runtime supports it and labels such messages as peer. The alternative is to drop the setting and close the path. This is the repo owner's call, because they may be using it. The lifecycle entry passes or omits the setting according to the recorded answer.

### Other risks

- **Durability has a price.** The agent host, the intent log, the snapshot and replay exist only so that an engine restart does not end a turn or lose a queued message. Hosting the adapter as a child of the engine would remove two entries and most of a third, at the cost of losing the turn in flight and the coordinator's background agents on every engine update. This design keeps the tmux project's recorded goal. A reviewer who values the simpler shape more can overturn that here, before the host is built.
- **The adapter moves fast.** Version 0.110.0 shipped on the day this was written, and protocol version 2 is in Preview. Mitigation: pin a minimum version, target version 1 only, keep (c) reachable.
- **Version skew between a pinned adapter and an auto-updating `claude`.** The adapter's own source notes that a CLI it does not ship can fail at startup. The preflight and the inline failure state bound the damage. S13 saw no failure at a skew of two patch releases; larger skews are untested.
- **The turn boundary rests on an adapter extension.** The start and end of an agent-started turn are not ACP v1. If a later adapter renames or drops them, the queue loses its end signal. Mitigation: the preflight requires the capabilities, the reducer's fixtures pin the shapes, and the minimum adapter version is raised deliberately.
- **Boss takes on a rendering surface.** This is the cost the April decision avoided. Phase one bounds it to one generic tool row, and the dropped affordances are listed above. The risk is that the list grows by attrition.
- **Same-user trust.** The host socket and the generation directory are protected by file mode, like the tmux socket and the private input file today. A hostile same-user process is outside what either design defends against.
- **A wedged turn.** If the adapter never answers a cancel, the operator resets, as they would a wedged terminal. No automatic timeout is added, because a new deadline needs measured traffic first.
- **`PATH` inside the tmux server.** A Node-based adapter needs `node` there. The engine resolves an absolute command and the preflight checks it.
- **Overlap with native window tabs.** Both projects edit `workBossPanel` in `ContentView.swift` and the code around `BossPaneModel`. The chat panel has no single-viewer constraint, which simplifies that project's terminal ownership rules for the coordinator. Whichever lands second forward-ports the other.
- **The file-count limit.** The lifecycle and image entries are estimated at 22 to 23 files. If either passes 25 once its edits are known, stop and propose a split rather than grow it.

## Proposed implementation task breakdown

Breakdown size: 17 entries (14 in-scope, 3 deferred) — three new crates each land with their first logic (the protocol client inside the spike, the agent host, the conversation reducer), the size limit then separates four engine capabilities that would otherwise be two (reducer from queue and persistence, RPC surface from service), followed by the coordinator's launch and recovery, its handoff transport, four panel capabilities that share Swift files and so land in order, one isolation prerequisite found while reading the lifecycle code, and one acceptance run; the three deferred entries are the default flip that waits for a human soak, the removal that waits for the flip, and a packaging decision that waits for an answer.

Dependency graph: entries 1 and 2 start immediately and in parallel. Entry 3 follows 1. Entry 4 follows 3. Entries 5 and 6 follow 4 and run in parallel. Entry 7 follows 5 and 6. Entry 8 follows 7 and 2. Entries 9 and 10 follow 8 and run in parallel. Entries 11, 12 and 13 are sequential after 10. Entry 14 follows 9 and 13. The three deferred entries follow 14; entry 16 also names entry 15.

File overlap: entries 1 and 2 both edit the session-directory preparation in `Ghostty/BossPaneModel.swift`; entry 1 lands first and entry 2 forward-ports it, keeping both changes. Entries 3 and 4 each add a crate and are ordered, so the lockfiles do not conflict. Entries 5 and 6 share no files. Entries 8 and 9 both edit `engine/core/src/coordinator_tmux.rs` and `engine/app/src/app/sessions.rs`, which is why they are ordered. Entry 9 is engine-only and entry 10 is app-only, so they do not conflict. Entries 10 to 13 all edit `CoordinatorChatModel.swift`, and most of them `CoordinatorChatMessageViews.swift`, which is why they are ordered rather than parallel.

All paths below are relative to `tools/boss/` unless they are root files. App sources and tests are globbed by `app-macos/BUILD.bazel`, so new Swift files need no build edit.

### ACP coordinator spike: client crate, probe and isolated chat fixture

Scope: land the ACP client crate with a minimal real surface (typed schema from the official crate, line framing, a connection), a probe binary that holds a conversation with the real adapter as a child process and records the wire in the journal's shape, and an isolated-only fixture in the app that launches the probe by path and renders the conversation. Run the criteria in "The spike: a validating study" against the real adapter, the real `claude` and the real login, with `boss` pointed at an isolated engine, and record the result. This is a validating study of ACP; it does not compare options. The findings amend this design in the same change. No recordings are checked in. Generated lockfile churn is not counted in the line estimate.

Major deliverables:

1. The `boss-acp-client` crate and probe binary.
2. An isolated-only chat fixture in the app, with captures of a streamed answer, a tool call, a permission card and a pasted image.
3. A findings writeup answering S1 to S16 with journal excerpts and versions, and an amendment to this design that states the gate outcome and any consequent change to later entries.

Likely changed files:

- `acp_client/BUILD.bazel` (new)
- `acp_client/Cargo.toml` (new)
- `acp_client/src/lib.rs` (new)
- `acp_client/src/connection.rs` (new)
- `acp_client/src/bin/probe.rs` (new)
- `acp_client/tests/framing.rs` (new)
- `Cargo.toml`
- `Cargo.lock`
- `MODULE.bazel.lock`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatProbe.swift` (new)
- `app-macos/Sources/BossCoordinatorChatProbeCapture.swift` (new)
- `app-macos/Sources/BossRevealCapture.swift`
- `app-macos/Sources/Ghostty/BossPaneModel.swift`
- `app-macos/Tests/BossTests/CoordinatorChatProbeTests.swift` (new)
- `docs/investigations/coordinator-acp-spike.md` (new)
- `docs/designs/native-swiftui-coordinator-chat-panel.md`

Estimated size: ~16 files / ~1450 changed lines

Effort hint: `large`

Dependencies: none

Scope: in-scope

### Isolated coordinator session for non-production engines

Scope: make a coordinator started by a non-production engine use a session directory under that engine's state root, on both the engine side and the app side, and export the engine's socket path into the session environment so `boss` and `bossctl` run by that coordinator reach the engine that launched it. Production paths do not change. First confirm the gap by test, since it is known only from reading the code; if the test shows it is already closed somewhere this design missed, record that and reduce the entry to the test. Update the isolation document.

Major deliverables:

1. A state-root-derived session directory for non-production engines, in the engine and in the app.
2. The engine's socket path in the coordinator session environment.
3. Tests showing that a non-production engine never resolves the production session directory, and the updated isolation document.

Likely changed files:

- `engine/core/src/coordinator_tmux.rs`
- `engine/app/src/app.rs`
- `engine/app/src/app/sessions.rs`
- `engine/app/src/app/tests/coordinator_reset.rs`
- `app-macos/Sources/Ghostty/BossPaneModel.swift`
- `app-macos/Sources/BossEnginePaths.swift`
- `app-macos/Tests/BossTests/CoordinatorSessionDirectoryTests.swift` (new)
- `docs/designs/test-instance-isolation.md`

Estimated size: ~8 files / ~350 changed lines

Effort hint: `small`

Dependencies: none

Scope: in-scope

### Durable agent host

Scope: add the `boss-agent-host` binary that the coordinator's tmux pane will run. It spawns a child with piped stdio, relays lines between the child and one client on a Unix socket, journals every line in both directions, replays from a requested sequence number on connect, lets a new connection replace an older one, mirrors the child's stderr to its own terminal, and exits with the child's status. It knows nothing about ACP; the journal record type lives in this crate. Bundle and sign it with Boss's other binaries and add its resolver beside `resolve_boss_cli`. It ships unused until the lifecycle entry. Tests use a scripted line-speaking child, kill the client mid-stream, and show that nothing is lost or duplicated on reconnect.

Major deliverables:

1. The `boss-agent-host` crate and binary with relay, journal, replay and the single-client rule.
2. The binary bundled, signed and resolvable by the engine.
3. A scripted child test helper that replays a recorded journal, reusable by later entries.

Likely changed files:

- `agent-host/BUILD.bazel` (new)
- `agent-host/Cargo.toml` (new)
- `agent-host/src/main.rs` (new)
- `agent-host/src/lib.rs` (new)
- `agent-host/src/relay.rs` (new)
- `agent-host/src/journal.rs` (new)
- `agent-host/tests/relay.rs` (new)
- `agent-host/tests/support/scripted_child.rs` (new)
- `Cargo.toml`
- `Cargo.lock`
- `MODULE.bazel.lock`
- `app-macos/BUILD.bazel`
- `installer/BUILD.bazel`
- `installer/release.sh`
- `engine/worker-bin/src/lib.rs`

Estimated size: ~15 files / ~1000 changed lines

Effort hint: `medium`

Dependencies: ACP coordinator spike: client crate, probe and isolated chat fixture

Scope: in-scope

### Chat item vocabulary and conversation reducer

Scope: add the chat item types to the protocol crate (types only, no request or event) and a new engine crate holding the reducer from wire-journal records to connection state and ordered chat items, the in-progress message snapshot, and credential redaction over every text field including streamed text with a held-back tail. The reducer also reads what the spike found outside the typed schema: the adapter's task updates that open and close an agent-started turn, and the agent's `$/cancel_request`. Fixtures are short, scrubbed excerpts recorded with the spike's probe: no credential, no home paths, no prompt text beyond markers, at most three files of about eighty lines each. Tests cover a background-agent completion, a permission request, a cancel, a truncated journal, a credential in a tool input, and a credential split across two chunks.

Major deliverables:

1. Chat item types in `boss-protocol`.
2. The reducer, deterministic under replay, with recorded-journal tests.
3. Redaction across all item kinds and the message in progress, with tests.

Likely changed files:

- `engine/coordinator-chat/BUILD.bazel` (new)
- `engine/coordinator-chat/Cargo.toml` (new)
- `engine/coordinator-chat/src/lib.rs` (new)
- `engine/coordinator-chat/src/reducer.rs` (new)
- `engine/coordinator-chat/src/redact.rs` (new)
- `engine/coordinator-chat/src/reducer_tests.rs` (new)
- `engine/coordinator-chat/src/redact_tests.rs` (new)
- `engine/coordinator-chat/testdata/*.jsonl` (~3 files)
- `protocol/src/types/coordinator_chat.rs` (new)
- `protocol/src/types.rs`
- `agent-host/BUILD.bazel`
- `acp_client/BUILD.bazel`
- `Cargo.toml`
- `Cargo.lock`
- `MODULE.bazel.lock`

Estimated size: ~17 files / ~1400 changed lines

Effort hint: `medium`

Dependencies: Durable agent host

Scope: in-scope

### Turn queue, intent log and replay snapshot

Scope: add what makes the conversation survive an engine restart. The turn queue with its origins, ordering, withdraw and cancel rules, including returning queued operator messages to the composer on Stop. The engine's intent log: append-only records of each prompt queued, sent and withdrawn, with origin, client message id and image reference. The join of the intent log with the wire journal in the reducer, so replay recovers the queue and each prompt's origin. A versioned reducer snapshot that a restart loads before replaying the tail, treated as a cache. Measure replay time on a synthetic journal sized from the spike's real per-turn sizes and state it.

Major deliverables:

1. The turn queue state machine with tests for ordering, engine priority, withdraw and cancel.
2. The intent log and its join with the wire journal, with a test that a queued message survives a simulated restart.
3. The snapshot, with tests for a missing, corrupt and version-mismatched snapshot, and the measured replay time.

Likely changed files:

- `engine/coordinator-chat/src/lib.rs`
- `engine/coordinator-chat/src/queue.rs` (new)
- `engine/coordinator-chat/src/queue_tests.rs` (new)
- `engine/coordinator-chat/src/intent.rs` (new)
- `engine/coordinator-chat/src/intent_tests.rs` (new)
- `engine/coordinator-chat/src/snapshot.rs` (new)
- `engine/coordinator-chat/src/reducer.rs`
- `engine/coordinator-chat/src/reducer_tests.rs`
- `engine/coordinator-chat/testdata/*.jsonl` (~1 files)

Estimated size: ~9 files / ~1050 changed lines

Effort hint: `medium`

Dependencies: Chat item vocabulary and conversation reducer

Scope: in-scope

### Coordinator chat RPC surface

Scope: declare the one request (`CoordinatorChat` with send, cancel, answer-permission and fetch operations), its result event, and the one topic (`coordinator.chat`) with its payload, and make every exhaustive match and allowlist know them. The request is app-only at the tier gate and in the handler. The handler in this entry answers that the chat backend is not running. This is separate from the service only because together they pass the file limit; it is checked by wire tests, the tier matrix and a handler test.

Major deliverables:

1. The request, its result event and the topic payload, with wire tests.
2. App-only enforcement at the tier gate and in the handler, with tier-matrix coverage.

Likely changed files:

- `protocol/src/wire.rs`
- `protocol/src/wire/coordinator_chat.rs` (new)
- `protocol/src/wire/coordinator_chat_tests.rs` (new)
- `protocol/src/wire/events.rs`
- `protocol/src/wire/topics.rs`
- `protocol/src/wire/topic_event_payload.rs`
- `protocol/src/wire/topic_and_envelope_tests.rs`
- `protocol/src/request_contract.rs`
- `client/src/replay.rs`
- `engine/app/src/app.rs`
- `engine/app/src/app/dispatch.rs`
- `engine/app/src/app/coordinator_chat.rs` (new)
- `engine/app/src/app/frontend_event_kind.rs`
- `engine/app/src/app/read_admission/lane.rs`
- `engine/app/src/app/tests.rs`
- `engine/app/src/app/tests/coordinator_chat.rs` (new)
- `engine/worker-policy/src/policy.rs`
- `engine/worker-policy/src/sanitize.rs`
- `engine/worker-policy/src/tests.rs`
- `engine/core/tests/comments_crud.rs`
- `engine/core/tests/work_crud.rs`

Estimated size: ~21 files / ~600 changed lines

Effort hint: `medium`

Dependencies: Chat item vocabulary and conversation reducer

Scope: in-scope

### Engine chat service

Scope: add the service, in the `engine/coordinator-chat` crate, that connects to an agent host socket, verifies its peer, runs the reducer live, drives the queue, writes the intent log before acknowledging a send, publishes items and offset-carrying deltas, serves fetches, and creates and prunes generation directories by a measured policy. Wire the RPC handler to it. An integration test runs the real agent host with the scripted child and exercises send, streaming, a permission answer, cancel, fetch after a dropped push, and a reconnect with replay in which a queued message survives. No coordinator lifecycle change: nothing starts this service in production yet.

Major deliverables:

1. The chat service wired to the host socket, reducer, queue and intent log, behind the RPC handler.
2. Generation directory creation and pruning, with the measurement that sets the policy.
3. An integration test through the real agent host covering reconnect, replay and redaction on the publish path.

Likely changed files:

- `engine/coordinator-chat/BUILD.bazel`
- `engine/coordinator-chat/Cargo.toml`
- `engine/coordinator-chat/src/lib.rs`
- `engine/coordinator-chat/src/service.rs` (new)
- `engine/coordinator-chat/src/retention.rs` (new)
- `engine/coordinator-chat/tests/service.rs` (new)
- `engine/app/src/app.rs`
- `engine/app/src/app/coordinator_chat.rs`
- `engine/app/src/app/tests/coordinator_chat.rs`
- `engine/app/BUILD.bazel`
- `engine/app/Cargo.toml`
- `agent-host/BUILD.bazel`
- `acp_client/BUILD.bazel`
- `Cargo.lock`

Estimated size: ~14 files / ~1300 changed lines

Effort hint: `large`

Dependencies: Turn queue, intent log and replay snapshot; Coordinator chat RPC surface

Scope: in-scope

### Coordinator lifecycle on the chat backend

Scope: make the chat backend launchable behind the default-off `coordinator_native_chat` flag, whose description says it is not usable outside isolated engines until the panel can send and answer permissions. `start_new` selects the pane command by backend; a preflight checks the agent host, the adapter command and `claude`; the engine initialises the session with the extension capabilities, sets the model as a session option, selects auto mode, applies the recorded answer on the cross-session setting, and queues the brief from the private input home as the first prompt. Adoption after an engine restart reconnects and replays. A Claude Code process that dies under a live adapter leaves the pane alive, so it is detected from the prompt error the adapter returns. A failed preflight, initialise or session start, a login-required error or an adapter-reported session failure counts toward the existing restart counters; there is no automatic fallback. The engine publishes the backend's state and failure reason, and the attach request tells the app which backend is live; until the panel lands the app shows an inline placeholder. Check the coordinator prompt for wording that assumes a terminal or names the inbox and make it backend-neutral without forking it. Amend `main.md` in the same change. Add a `manual` live test target with the real adapter, run under an isolated engine.

Major deliverables:

1. Backend-selected launch with preflight, session setup and brief delivery.
2. Recovery: reconnect and replay on adoption, failure accounting against the existing ceiling, and a published failure reason.
3. The backend field on the attach contract, consumed by the app.

Likely changed files:

- `engine/core/src/coordinator_tmux.rs`
- `engine/core/src/coordinator_tmux/chat_backend.rs` (new)
- `engine/core/src/coordinator_tmux/chat_backend_tests.rs` (new)
- `engine/core/src/config.rs`
- `engine/core/runtime/Cargo.toml`
- `engine/core/runtime.bzl`
- `engine/core/BUILD.bazel`
- `engine/core/tests/coordinator_chat_live.rs` (new)
- `engine/coordinator-chat/BUILD.bazel`
- `engine/work-db/src/coordinator_tmux.rs`
- `engine/feature-flags/src/lib.rs`
- `engine/app/src/app/sessions.rs`
- `engine/app/src/app/server.rs`
- `engine/app/src/app/server/startup_attentions.rs`
- `engine/app/src/app/coordinator_chat.rs`
- `protocol/src/engine_app.rs`
- `Cargo.lock`
- `app-macos/Sources/EngineProtocolTypes.swift`
- `app-macos/Sources/EngineClient.swift`
- `app-macos/Sources/Ghostty/BossPaneModel.swift`
- `app-macos/Sources/ContentView.swift`
- `app-macos/Tests/BossTests/CoordinatorBackendAttachTests.swift` (new)
- `docs/designs/main.md`

Estimated size: ~23 files / ~1350 changed lines

Effort hint: `large`

Dependencies: Engine chat service; Isolated coordinator session for non-production engines

Scope: in-scope

### Handoff and engine prompts on the chat backend

Scope: implement `HandoffDelivery` for the chat backend so the reset request, the authorization delivery and redelivery, and the prompt-changed nudge travel as engine prompts through the queue, with the mapping of `deliver`, `deliver_accepted` and `turn_ended` given under "Handoff". Keep every id rule, timeout and error prefix unchanged, and withdraw a timed-out request from the queue. Record audit stages equivalent to the inbox delivery stages. Update the handoff document to describe both transports. Add a `manual` live test for a reset with a fresh handoff on the real adapter, run under an isolated engine.

Major deliverables:

1. The chat backend's delivery implementation for reset, authorization and nudge.
2. The existing reset tests run against both backends, including a request made mid-turn and a timed-out request that is never delivered late.
3. The updated handoff document and the live test target.

Likely changed files:

- `engine/core/src/coordinator_tmux.rs`
- `engine/core/src/coordinator_tmux/inbox.rs`
- `engine/core/src/coordinator_tmux/chat_delivery.rs` (new)
- `engine/core/src/coordinator_tmux/chat_delivery_tests.rs` (new)
- `engine/core/src/coordinator_tmux/reset_handoff.rs`
- `engine/core/BUILD.bazel`
- `engine/core/tests/coordinator_chat_handoff_live.rs` (new)
- `engine/app/src/app/coordinator_handoff.rs`
- `engine/app/src/app/sessions.rs`
- `engine/app/src/app/tests/coordinator_reset.rs`
- `docs/coordinator-session-handoff.md`

Estimated size: ~11 files / ~850 changed lines

Effort hint: `medium`

Dependencies: Coordinator lifecycle on the chat backend

Scope: in-scope

### Chat panel: conversation rendering

Scope: build the read side of the panel and put it in `workBossPanel` when the chat backend is live. A new `CoordinatorChatModel` subscribes to the topic, fetches the backlog, applies deltas by offset and re-fetches on a gap. The list renders operator, coordinator, engine and peer messages with the Boss markdown theme, a rate-bounded streaming tail, turn boundaries, notices (session start, compaction, error, login required), a generic collapsed row for any other item kind, work-item links that send `RevealWorkItem` (confirm the app session is admitted to that request, which was written for the coordinator, and admit it if not), and tail-follow with "jump to latest". When the backend could not start, the panel shows the reason and the "Switch to terminal coordinator" action. Tool calls appear as the generic row in this entry. No input yet: the brief and the coordinator's first reply are what it shows. Adds a capture scenario from fixture items.

Major deliverables:

1. `CoordinatorChatModel` with subscribe, fetch, delta application and resync, and its decoding in the engine client.
2. The message list with markdown, streaming tail, notices and work-item links, mounted in the Work-mode panel by backend.
3. The failure state with its switch action, a capture scenario, and tests for decoding, resync and link detection.

Likely changed files:

- `app-macos/Sources/CoordinatorChat/CoordinatorChatModel.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatPanel.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatMessageViews.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatLinks.swift` (new)
- `app-macos/Sources/ChatViewModel+CoordinatorChat.swift` (new)
- `app-macos/Sources/ChatViewModel+EventHandling.swift`
- `app-macos/Sources/EngineClient.swift`
- `app-macos/Sources/EngineClient+Requests.swift`
- `app-macos/Sources/EngineClient+Parsers.swift`
- `app-macos/Sources/EngineEvent.swift`
- `app-macos/Sources/ContentView.swift`
- `app-macos/Sources/BossCoordinatorChatCapture.swift` (new)
- `app-macos/Sources/BossRevealCapture.swift`
- `app-macos/Tests/BossTests/CoordinatorChatModelTests.swift` (new)
- `app-macos/Tests/BossTests/CoordinatorChatDecodingTests.swift` (new)
- `app-macos/Tests/BossTests/CoordinatorChatLinksTests.swift` (new)

Estimated size: ~16 files / ~1350 changed lines

Effort hint: `large`

Dependencies: Coordinator lifecycle on the chat backend

Scope: in-scope

### Chat panel: composer, send, queue and stop

Scope: add the input side. A multi-line composer sends on Return, inserts a newline on Shift-Return, and keeps its draft across panel collapse and app-side reconnects. Sent messages appear immediately as pending, then as queued or delivered from engine state. A status row shows idle, working, queued and waiting-for-permission, context usage when reported, and a Stop control with Escape bound to cancel; after a Stop, queued messages come back into the composer. Text beginning with a slash is sent unchanged. Ideas "Send to Coordinator" uses the same send operation when the chat backend is live. Remove the spike's app fixture, which the real panel replaces; the probe binary stays as a manual harness. The send path must not reintroduce identifiers that `tmux_only_surface_test` bans.

Major deliverables:

1. The composer and the send operation with pending, queued and failed states shown inline.
2. The status row with context usage, Stop and Escape cancel.
3. Ideas "Send to Coordinator" routed through the chat send on the chat backend, and the spike fixture removed.

Likely changed files:

- `app-macos/Sources/CoordinatorChat/CoordinatorChatComposer.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatStatusRow.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatModel.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatPanel.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatProbe.swift`
- `app-macos/Sources/BossCoordinatorChatProbeCapture.swift`
- `app-macos/Sources/BossRevealCapture.swift`
- `app-macos/Sources/EngineClient+Requests.swift`
- `app-macos/Sources/ContentView.swift`
- `app-macos/Sources/BossCoordinatorChatCapture.swift`
- `app-macos/Tests/BossTests/CoordinatorChatProbeTests.swift`
- `app-macos/Tests/BossTests/CoordinatorChatComposerTests.swift` (new)
- `app-macos/Tests/BossTests/CoordinatorChatSendStateTests.swift` (new)

Estimated size: ~13 files / ~1200 changed lines

Effort hint: `medium`

Dependencies: Chat panel: conversation rendering

Scope: in-scope

### Chat panel: tool activity and permission requests

Scope: replace the generic row for tool calls with a proper tool row (title, kind, status, running indicator, expandable bounded input and output with a truncation marker, a distinct label for a call blocked by auto mode) and collapsed thinking. Render a permission request as a card with the agent's options, send the answer, show the resolution, and show requests that were cancelled with their turn. A request that was already pending when the app connected is shown as pending. Once this lands the flag's description is updated to say the backend is usable.

Major deliverables:

1. Tool rows and collapsed thinking.
2. Permission cards with answer, resolved and cancelled states.
3. Capture states and tests for each tool status and each permission outcome.

Likely changed files:

- `app-macos/Sources/CoordinatorChat/CoordinatorChatToolViews.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatPermissionCard.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatModel.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatPanel.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatMessageViews.swift`
- `app-macos/Sources/EngineClient+Requests.swift`
- `app-macos/Sources/BossCoordinatorChatCapture.swift`
- `app-macos/Tests/BossTests/CoordinatorChatToolRowTests.swift` (new)
- `app-macos/Tests/BossTests/CoordinatorChatPermissionTests.swift` (new)
- `engine/feature-flags/src/lib.rs`

Estimated size: ~10 files / ~900 changed lines

Effort hint: `medium`

Dependencies: Chat panel: composer, send, queue and stop

Scope: in-scope

### Chat panel: image attachments

Scope: let the operator paste or drop images into the composer and send them with a message. The app writes the image to a file and names it in the send operation; the engine validates it with the attachment crate's format sniffing, stores it in the generation directory, records the reference in the intent log, and sends it as an ACP image block. The sent message shows a thumbnail after a restart, through a file-path entry point added to `AttachmentImageCache`. The attachment store and its rows are not used. Measure real screenshot sizes before setting the cap, and state the measurement. Rejections (format, size, dimensions) appear inline on the chip. If the footprint passes 25 files, stop and propose a split between the engine half and the app half.

Major deliverables:

1. Composer paste and drop with chips, and attachments on the send operation.
2. Engine validation, storage in the generation directory and delivery as image blocks, with typed rejections.
3. Thumbnails on sent messages from stored references, with tests on both sides.

Likely changed files:

- `protocol/src/types/coordinator_chat.rs`
- `protocol/src/wire/coordinator_chat.rs`
- `engine/coordinator-chat/BUILD.bazel`
- `engine/coordinator-chat/Cargo.toml`
- `engine/coordinator-chat/src/images.rs` (new)
- `engine/coordinator-chat/src/images_tests.rs` (new)
- `engine/coordinator-chat/src/intent.rs`
- `engine/coordinator-chat/src/queue.rs`
- `engine/coordinator-chat/src/reducer.rs`
- `engine/coordinator-chat/src/service.rs`
- `engine/attachments/BUILD.bazel`
- `engine/app/src/app/coordinator_chat.rs`
- `engine/app/src/app/tests/coordinator_chat.rs`
- `Cargo.lock`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatAttachments.swift` (new)
- `app-macos/Sources/CoordinatorChat/CoordinatorChatComposer.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatMessageViews.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorChatModel.swift`
- `app-macos/Sources/EngineClient+Requests.swift`
- `app-macos/Sources/AttachmentViewerView.swift`
- `app-macos/Sources/BossCoordinatorChatCapture.swift`
- `app-macos/Tests/BossTests/CoordinatorChatAttachmentTests.swift` (new)

Estimated size: ~22 files / ~1100 changed lines

Effort hint: `medium`

Dependencies: Chat panel: tool activity and permission requests

Scope: in-scope

### Parity acceptance run and operator documentation

Scope: run every item of the parity list on the real path (isolated engine with an isolated session, real agent host, real adapter, real login) and record what was observed for each, with captures and journal excerpts, distinguishing observed results from expectations. Input is driven by an isolated app instance in a scripted mode added here: it registers as the app, sends a fixed list of messages and permission answers, and captures at each step. An item that needs a person, such as pasting from the real clipboard, is marked as such and left for the repo owner. Anything that fails is reported as a gap against a named entry, not fixed here. Write the operational document for the chat backend (processes, files, the two logs, how to read a failure) and the fallback runbook, link them from the root `CLAUDE.md` list, and refresh this design's status section. This run can block the default flip that follows it; it has no authority over the entries before it, which is why each of those carries its own acceptance.

Major deliverables:

1. A scripted isolated-app mode and a parity run report covering all eleven items with evidence.
2. The operational document and the fallback runbook.
3. The refreshed status section of this design, with any remaining gaps named.

Likely changed files:

- `docs/investigations/coordinator-chat-parity-run.md` (new)
- `docs/coordinator-chat-panel.md` (new)
- `docs/runbooks/coordinator-chat-fallback.md` (new)
- `docs/designs/native-swiftui-coordinator-chat-panel.md`
- `CLAUDE.md`
- `app-macos/Sources/BossCoordinatorChatCapture.swift`
- `app-macos/README.md`

Estimated size: ~7 files / ~700 changed lines

Effort hint: `medium`

Dependencies: Handoff and engine prompts on the chat backend; Chat panel: image attachments

Scope: in-scope

### Make the chat backend the default

Scope: flip the default of `coordinator_native_chat` to on, update the flag's description and tests, and note the date in the operational document. The terminal backend stays selectable by turning the flag off.

Major deliverables:

1. The default flipped, with the flag's tests and description updated.

Likely changed files:

- `engine/feature-flags/src/lib.rs`
- `docs/coordinator-chat-panel.md`
- `docs/designs/native-swiftui-coordinator-chat-panel.md`

Estimated size: ~3 files / ~40 changed lines

Effort hint: `trivial`

Dependencies: Parity acceptance run and operator documentation

Scope: deferred (future / not a v1 blocker) — waits for the repo owner's soak on the chat backend after the parity run, which a worker cannot supply; if bundling the adapter is chosen, that lands first.

### Retire the terminal coordinator backend

Scope: remove what only the terminal coordinator uses: the coordinator's use of the terminal relay and of Claude Code's inbox for delivery, the `send_keys` nudge, the Ghostty coordinator viewer and its reattach loop, the tmux identity fields on the attach contract, the flag, and the inbox live tests. `BossPaneModel.swift` is the terminal viewer's model and goes with it, but it also holds the system prompt and the session-directory preparation, which must survive; move those to a new file and update everything that names the old one, including the build consumers and the root `BOSS_COORDINATOR.md`. Keep the tmux session, the supervisor and the handoff store. Update the handoff and operational documents.

Major deliverables:

1. Terminal-only delivery and viewer code removed, with the flag.
2. The prompt source relocated, with its build consumers, tests and references updated.
3. Documents and tests that described two backends reduced to one.

Likely changed files:

- `engine/core/src/coordinator_tmux.rs`
- `engine/core/src/coordinator_tmux/*.rs` (~6 files)
- `engine/core/BUILD.bazel`
- `engine/feature-flags/src/lib.rs`
- `engine/app/src/app/sessions.rs`
- `engine/app/src/app/coordinator_handoff.rs`
- `protocol/src/engine_app.rs`
- `cli/src/handoff.rs`
- `cli/BUILD.bazel`
- `app-macos/BUILD.bazel`
- `app-macos/Sources/Ghostty/BossPaneModel.swift`
- `app-macos/Sources/Ghostty/BossPaneTerminalView.swift`
- `app-macos/Sources/CoordinatorChat/CoordinatorPrompt.swift` (new)
- `app-macos/Sources/ContentView.swift`
- `app-macos/Sources/EngineProtocolTypes.swift`
- `app-macos/Sources/EngineClient.swift`
- `BOSS_COORDINATOR.md`
- `docs/coordinator-session-handoff.md`
- `docs/coordinator-chat-panel.md`

Estimated size: ~24 files / ~1400 changed lines

Effort hint: `large`

Dependencies: Parity acceptance run and operator documentation; Make the chat backend the default

Scope: deferred (future / not a v1 blocker) — the terminal backend is the fallback, so this must not start until the chat backend has been made the default and has soaked there.

### Bundle a pinned adapter with Boss

Scope: ship a pinned copy of the adapter's standalone executable inside the Boss distribution, fetched by version and checksum in the build, so a fresh install needs no separate adapter install. The engine prefers the bundled copy unless the setting names another. Report the size added to the installer. Decide, from the spike's skew result, whether the bundled copy runs the installed `claude` or its own.

Major deliverables:

1. The pinned adapter fetched and packaged, with the size reported.
2. Engine resolution of the bundled copy and the preflight updated.

Likely changed files:

- `MODULE.bazel`
- `MODULE.bazel.lock`
- `app-macos/BUILD.bazel`
- `installer/scripts/postinstall`
- `engine/core/src/config.rs`
- `engine/core/src/coordinator_tmux/chat_backend.rs`
- `engine/core/src/coordinator_tmux/chat_backend_tests.rs`
- `docs/coordinator-chat-panel.md`

Estimated size: ~8 files / ~350 changed lines

Effort hint: `medium`

Dependencies: Parity acceptance run and operator documentation

Scope: deferred (future / not a v1 blocker) — the plan of record requires the adapter installed on the machine; bundling waits for the provisioning answer and the spike's size measurement.
