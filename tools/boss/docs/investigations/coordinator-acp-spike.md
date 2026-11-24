# Coordinator ACP spike: protocol findings and live acceptance plan

- **Date:** 2026-11-24
- **Status:** Source investigation complete; live spike gate unevaluated.
- **Related design:** [Native SwiftUI coordinator chat panel](../designs/native-swiftui-coordinator-chat-panel.md), especially “The spike: a validating study.”
- **Repository baseline:** `e2381ab36c67b873ab5cde991efe7ef28c7b1402` on `main`.
- **Evidence:** Repository and pinned upstream source inspection, release metadata, and local executable version checks. No model conversation, wire recording, or UI capture was produced.

Boss's proposed native coordinator needs structured conversation updates while preserving Claude Code's tools, instructions, login and background work. This investigation checks the implementation seams and specifies the evidence still needed to validate ACP v1 through the Claude adapter.

## Verdict

**The ACP acceptance gate has not passed or failed: it has not been run.** Keep the terminal coordinator as the default and retain ACP v1 as the planned backend. Source inspection identifies two details to resolve in the live spike: background completion boundaries depend on negotiated adapter capabilities, and the Rust SDK exposes request IDs but does not automatically recover pending requests across a new connection.

This document delivers no client crate or app fixture. It supplies an evidence ledger, a proposed design amendment and the implementation follow-ups needed to obtain a gate result. Source behavior is not substituted for any required live observation.

## Method and reproducible baseline

The checkout was clean before investigation. Searches of the workspace manifest, lockfile and Boss file inventory found no `acp_client` crate, `CoordinatorChatProbe`, or `CoordinatorChat` source directory. The design's expected implementation files are therefore future work at this baseline, rather than an existing probe that could be exercised.

Repository inspection covered the design, coordinator launch and directory selection, app prompt/settings preparation, client socket discovery, capture infrastructure, and transcript tailing. Upstream inspection used immutable release commits, not the default branch. Local runtime checks were limited to executable discovery and versions; neither an engine nor an adapter conversation was launched. Login availability and account metering were not tested, and no credential failure is inferred.

| Component                 | Evidence observed                                                                                                    | Meaning                                                                                      |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Installed Claude Code     | `claude --version` returned `2.1.331 (Claude Code)`, exit 0                                                          | Executable version only; does not prove authentication or adapter compatibility.             |
| Node                      | `node -p 'process.version'` returned `v24.8.0`, exit 0                                                               | Meets the pinned package's declared Node requirement, not a startup test.                    |
| Adapter executable        | `command -v` found neither `claude-agent-acp` nor the historical `claude-code-acp` command                           | No adapter command on the inspected PATH; does not prove no installation exists elsewhere.   |
| Adapter source            | Release `v0.110.0`, commit `f9c25404ecebd2da2604608ac3be368784962236`                                                | The design's version, inspected but not installed or executed.                               |
| Adapter dependencies      | Package manifest pins `@anthropic-ai/claude-agent-sdk` `0.3.330` and `@agentclientprotocol/sdk` `2.9.1`; Node `>=22` | Declared versions, not versions observed in a running process.                               |
| Official Rust SDK         | `agent-client-protocol` release `v5.5.1`, commit `34aea54f7314b42563b09112fe40b0cd5eb29b8a`                          | Source inspected; no local dependency added or compiled.                                     |
| Standalone macOS archives | Release API reports arm64 ZIP 202,506,412 bytes and x64 ZIP 213,511,742 bytes                                        | Published compressed sizes, not downloaded sizes, installed footprint or signing validation. |

The adapter version, dependencies and runtime requirement come from its [pinned package manifest](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/package.json). Archive sizes come from the [release metadata](https://api.github.com/repos/agentclientprotocol/claude-agent-acp/releases/tags/v0.110.0); publication time is `2026-11-23T09:47:18Z`. The Rust revision comes from the [v5.5.1 release tag](https://github.com/agentclientprotocol/rust-sdk/tree/v5.5.1).

Useful read-only commands for repeating the source checks:

```sh
gh api repos/agentclientprotocol/claude-agent-acp/git/ref/tags/v0.110.0
gh api 'repos/agentclientprotocol/claude-agent-acp/contents/package.json?ref=v0.110.0' --jq .content | base64 -D
gh api 'repos/agentclientprotocol/rust-sdk/tags?per_page=100' --jq '.[] | select(.name == "v5.5.1")'
gh api repos/agentclientprotocol/claude-agent-acp/releases/tags/v0.110.0 --jq '.assets[] | {name,size}'
```

## Findings

### Background agents require an explicit capability profile

The adapter's default v1 behavior can keep `session/prompt` open while a background subagent runs. The `backgroundSubagents` AIR capability removes that hold, allowing the prompt response to precede the subagent's completion. Separately, `asyncTasks` enables an explicit autonomous-turn lifecycle. These are negotiated under `clientCapabilities._meta.jetbrains.air`, with version 1 and a capabilities array. They are adapter extensions, not the baseline ACP v1 contract. See [Background subagents and Async tasks](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/docs/air-extensions.md#background-subagents) and the [capability checks](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/air-extension.ts).

In source, `AsyncTaskRuntime.autonomousTurnStarted` returns immediately when that runtime is disabled. When enabled, it creates an `autonomous-turn-…` task with `taskType: "turn"`. The matching end transitions it to completed or failed. The adapter wires this runtime to negotiated `asyncTasks` support and calls its start/end methods from its SDK stream consumer. See [async-tasks.ts](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/async-tasks.ts#L483) and [the v1 stream consumer](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/v1/acp-agent.ts#L3263).

**Implication:** S6 needs the full capability request and response in its evidence, followed by the original prompt result, later root answer and correlated autonomous-turn end. Test the minimal v1 profile first, then a separately labelled profile requesting `backgroundSubagents` and `asyncTasks`. Keep `fs` and `terminal` absent in both. This preserves the chosen protocol and tool-execution boundary while exposing the extension dependency for review; it does not silently adopt v2.

A tool completing, a quiet interval, or a usage update is not sufficient evidence that the parent model has finished its follow-up. Also, unsolicited updates can be session metadata rather than an autonomous answer: ACP explicitly allows updates outside prompt turns. The reducer must classify them before opening a turn. See [ACP v1 prompt turns](https://agentclientprotocol.com/protocol/v1/prompt-turn).

### Request-ID visibility and recovery are separate SDK questions

The official Rust SDK provides `v1::ConnectionTo::prepare_request`, `PreparedRequest::id`, `SentRequest::id`, and incoming responder IDs. Boss can know a request ID before publication. A new connection nevertheless creates an empty `PendingReplies`; incoming responses absent from that table are logged and dropped. The request preparation path generates a fresh UUID. See [v1 connection APIs](https://github.com/agentclientprotocol/rust-sdk/blob/34aea54f7314b42563b09112fe40b0cd5eb29b8a/src/agent-client-protocol/src/jsonrpc/v1_connection.rs#L179), [connection and request construction](https://github.com/agentclientprotocol/rust-sdk/blob/34aea54f7314b42563b09112fe40b0cd5eb29b8a/src/agent-client-protocol/src/jsonrpc.rs), and [response dispatch](https://github.com/agentclientprotocol/rust-sdk/blob/34aea54f7314b42563b09112fe40b0cd5eb29b8a/src/agent-client-protocol/src/jsonrpc/incoming_actor.rs#L233).

**Implication:** “Request IDs are hidden” is not a reason to write a replacement transport. Retain the official schema and evaluate its connection for the child-process probe. S15 must separately demonstrate attaching to an initialized stream, recognizing an earlier process's unanswered request, and avoiding a second initialization or prompt. The inspected code does not establish a public pending-request restoration path; transport attachment alone would not satisfy recovery.

If restoration cannot be implemented through the public API, keep the necessary correlation/replay layer small and document that specific gap. Do not resend an old prompt merely to manufacture a pending entry: it can repeat model work and tool effects. This is a source-level recommendation, not a compiled proof of the final transport choice.

### A fixture must render its own session directory

The app's `ensureBossWorkingDirectory` constructs the fixed Application Support coordinator directory and immediately writes `CLAUDE.md` and local settings. The engine's `coordinator_working_directory` selects the same production location. `ServerState::coordinator_session_directory` changes that only when its explicit override is present. The launch environment assembled in `start_new` does not explicitly insert `BOSS_SOCKET_PATH`, although the CLI's `Discovery::from_env` reads it.

These observations confirm the design's source-level isolation concern. They do not demonstrate a production leak at runtime. Sources: [app preparation](../../app-macos/Sources/Ghostty/BossPaneModel.swift), [coordinator launch](../../engine/core/src/coordinator_tmux.rs), [server directory selection](../../engine/app/src/app.rs), and [client discovery](../../client/src/lib.rs).

The spike fixture should reuse the actual prompt/settings renderer with an injected temporary destination. It must not first call the production-writing helper and then copy its output. Pass the isolated engine socket explicitly to the probe and adapter child, and keep inherited production event routing out of that process tree. Verify the selected engine with a read-only `boss` request before prompting the model. Do not trigger the normal coordinator supervisor as a shortcut to constructing the fixture session.

The existing [quiet capture implementation](../../app-macos/Sources/BossRevealCapture.swift) and [fixture path selection](../../app-macos/Sources/BossEnginePaths.swift) are integration points. Their existence does not prove an ACP fixture exists or is isolated. The separate coordinator isolation work remains necessary before normal lifecycle adoption.

### Settings, mode and CLI selection need observations, not assumptions

`NewSessionMeta` accepts `_meta.claudeCode.options`, but documents several options as adapter-managed, including `permissionMode`, `cwd` and `canUseTool`. The v1 implementation supplies user, project and local setting sources. Select the requested mode through ACP and record the reported effective mode and model: the mode manager can fall back when auto mode is unavailable for a model. Sources: [new-session metadata](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/new-session-meta.ts), [session creation](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/v1/acp-agent.ts), and [mode manager](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/session-mode.ts).

`CLAUDE_CODE_EXECUTABLE` takes precedence over the packaged CLI. The pinned adapter also changes settings/MCP delivery according to that choice: an explicitly selected CLI uses command-line delivery by default, while the shipped CLI uses initialization delivery. S13 therefore needs two real startup observations, even if both binaries report the same version. See [CLI selection and delivery](https://github.com/agentclientprotocol/claude-agent-acp/blob/f9c25404ecebd2da2604608ac3be368784962236/src/claude-cli.ts).

### Reuse boundaries

Use the official SDK's schema and existing connection facilities where they meet the requirements. Boss's [transcript-tail crate](../../engine/transcript-tail/src/lib.rs) already handles partial JSONL file tails and file rotation; that is relevant to journal consumption, but is not a duplex ACP request connection. [claude_client](../../claude_client/src/lib.rs) implements Messages API requests with API-key authentication and is not a Claude Code subprocess transport. Reuse the app's prompt/settings and capture code rather than duplicating their content in the probe.

## S1–S16 evidence ledger

“Not run” means no qualifying runtime evidence, not a failed criterion. S1–S8 are the required gate observations; the optional classifier-denial part of S3 remains optional. S9–S16 inform later implementation decisions. No row below claims a live pass.

| Criterion                      | Result in this investigation                                                        | Evidence needed to close it                                                                                                                                                                                         |
| ------------------------------ | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| S1: connection and three turns | Not run. Pinned versions and local CLI version are recorded above.                  | Successful initialize/session creation and three prompt responses, with streamed chunks before each turn ends; running adapter, SDK and CLI versions.                                                               |
| S2: prompt, rules and memory   | Not run. Production-writing helper and setting sources inspected.                   | Rendered fixture prompt is in effect; an allowed read-only `boss` command reaches the isolated engine without a permission request; a unique memory marker from the fixture cwd's derived memory store is recalled. |
| S3: auto mode and model        | Not run. Source permits model-dependent fallback.                                   | Effective `auto` mode and coordinator model confirmed after selection; capture classifier-denial shape if safely obtainable.                                                                                        |
| S4: allow and reject           | Not run; no ACP permission fixture exists at this baseline.                         | Separate allow/reject answers sent from the fixture using offered option IDs, with the resulting tool behavior and request correlation.                                                                             |
| S5: cancellation               | Not run.                                                                            | Cancel during an active turn, resolve pending permission requests as cancelled, observe the terminal prompt response, then complete a new prompt.                                                                   |
| S6: background completion      | Not run. Capability-dependent behavior identified above.                            | Prompt ends before background subagent; later root answer arrives without another prompt; explicit start/end signals correlate. Record baseline and extended v1 capability profiles separately.                     |
| S7: PNG paste                  | Not run; no image capture or model response.                                        | Paste a PNG into the fixture, send image content after capability negotiation, and record a description of image-specific details.                                                                                  |
| S8: Claude plan login          | Not run; `claude --version` is not an authentication check.                         | Complete real prompts with `ANTHROPIC_API_KEY` unset, record the selected authentication route without secrets, and identify any other provider overrides. Runtime success does not settle account policy.          |
| S9: concurrent prompt          | Not run.                                                                            | Send a second prompt mid-turn in a dedicated probe case; correlate both request IDs and record queueing, steering or rejection. Keep the production plan's single-flight queue until evidence supports a change.    |
| S10: peer inbox                | Not run.                                                                            | Check whether the fixture's CLI registers an inbox; if available, send one isolated peer message and trace its provenance and updates. Absence is a recorded result.                                                |
| S11: Claude transcript         | Not run; no transcript created.                                                     | Locate the transcript for the actual fixture cwd/session and exercise `engine/transcript-markdown` against it via Bazel tests. Record parse result and any unsupported entries.                                     |
| S12: slash commands            | Not run.                                                                            | Submit `/compact`, `/help` and `/status` as prompts; record emitted updates, prompt completion or error, and whether each is supported.                                                                             |
| S13: CLI version skew          | Not run. CLI-selection source inspected.                                            | Run the same startup/conversation smoke case with the explicit installed CLI and with the adapter's copy; record exact paths, versions, stderr and exit status.                                                     |
| S14: rendering cost            | Not run; no fixture or measurements.                                                | Long streamed answer containing code blocks: report delta count/size, markdown parse and main-thread render costs, sample count, machine and build configuration. Derive batching interval from measurements.       |
| S15: SDK recovery              | Source inspection only: IDs exposed; a fresh pending table drops unmatched replies. | Executable test of fresh-client attachment to an initialized stream with an old request outstanding, plus a new request and permission response. No duplicate initialize or resend.                                 |
| S16: child crash               | Not run; no child process killed.                                                   | Kill only the probe-owned Claude child during a turn; record adapter survival/exit, stderr, wire error/end events, outstanding permission disposition and response to the next prompt.                              |

## Evidence collection contract for the live spike

The probe must append each complete incoming and outgoing wire line to the design's `seq`, `ts_ms`, `dir`, `line` journal envelope. Use a single ordered writer; preserve the original JSON line and its request IDs rather than logging a reserialized projection. State the direction convention and timestamp clock in the run manifest. Keep adapter stderr separate from protocol stdout.

No journal excerpts are included here because no wire was recorded. The live report should quote short redacted excerpts with sequence ranges, request/option IDs and event order for every criterion; label omissions and never turn illustrative frames into observed evidence. Record permissions and image behavior from the same conversation shown in the app. Keep full journals, prompt text, local paths and image payloads out of the repository.

Capture and register four views: a partially streamed answer, tool activity/result, an unresolved permission card, and a pasted image with the model's description. Captions must say what was verified. A screenshot of canned data can validate layout, but cannot close a criterion requiring the adapter, real model or fixture interaction.

After S1, prioritize S6 because its boundary affects the queue and reducer. Then exercise settings/memory/login, permissions, cancel and image paste, followed by the recorded criteria. Use a fresh fixture session for experiments that interfere with each other, such as concurrent prompting and child termination. Record the protocol/capability profile on every session.

If a required criterion fails, reproduce that behavior against direct Claude stream-json as the design prescribes. This is failure attribution, not an options comparison. A missing probe, a source warning or an untested criterion is not sufficient evidence to choose a fallback.

## Proposed design amendment

Apply the following clarification to the design when the executable spike supplies its findings:

1. **Gate status:** Source review leaves the live acceptance gate unevaluated. Later entries gated on a successful spike do not gain acceptance from this report; terminal remains the default.
2. **Session capabilities:** S6 records both minimal ACP v1 behavior and the documented `backgroundSubagents`/`asyncTasks` profile. Any accepted extension becomes an explicit, pinned dependency of the queue/reducer contract. Neither profile advertises client filesystem or terminal execution.
3. **Client reuse:** Official Rust SDK request IDs are accessible. The reason to add correlation/framing code, if still needed after S15, is recovery of an earlier connection's requests, not lack of ID visibility.
4. **Turn classification:** A notification outside a prompt does not by itself open an autonomous turn. Distinguish ordinary session metadata, background tool activity and root model work; require a measured end signal.
5. **Isolation:** The fixture renders prompt/settings directly into its temporary cwd and explicitly routes Boss CLI commands to its isolated engine. Normal coordinator lifecycle adoption still depends on the separate isolation change.

These clarifications preserve the chosen backend and the existing proceed/direct-CLI-diagnostic/fallback gate. They do not claim engine restart durability, handoff correctness, app restart recovery or production parity, which the design assigns to later acceptance work.

## Follow-up implementation

The executable spike should land the following coherent change after this source report:

1. Add `tools/boss/acp_client/{Cargo.toml,BUILD.bazel,src/lib.rs,src/connection.rs,src/bin/probe.rs,tests/framing.rs}` and the workspace dependency/lockfile entries. Start with official typed schema and the SDK connection; keep replay-specific code at a narrow boundary. Give Bazel targets only the visibility their consumers require.
2. Refactor app session preparation to accept an explicit fixture destination. Add `CoordinatorChatProbe.swift`, `BossCoordinatorChatProbeCapture.swift` and their tests, then connect them to the existing quiet capture entry. Launch the probe by an explicit path and reject production use. Render streamed text, generic tool rows, offered permission choices and image attachments.
3. Test fragmented and multiple lines, UTF-8 boundaries, malformed frames, EOF with outstanding requests, interleaved responses, bidirectional request IDs and cancellation. Add the S15 recovery case before committing to custom framing. Run tests only through Bazel and use the repository checkleft gate.
4. Exercise the real adapter and login against the isolated engine; replace the ledger's untested results with observations, register captures, and update the design's status/evidence section with the actual gate outcome. Check in no recordings.

The design estimates about 16 files for that implementation. Recheck the footprint before coding because a separate earlier findings document changes which files already exist. Keep production coordinator directory/socket isolation as its own capability, in the separate change already described by the design; the spike's injected fixture path does not complete that work.

## Open questions

- Can the pinned v1 extension profile demonstrate a reliable autonomous-turn end with the real installed CLI? Source supports a candidate contract; only S6 can establish the gate result.
- Can a public SDK integration recover old pending requests without resending them, or must Boss own that narrow layer? S15 decides through a runnable recovery test.
- Is headless coordinator plan usage acceptable to the repo owner under the design's account-policy decision? No account conclusion or approval is inferred from a version check. Record that decision before the live plan-backed study.
- Peer-inbox policy and adapter packaging remain the design's later product decisions. This report supplies the background capability caveat and published archive sizes, without making those decisions.
