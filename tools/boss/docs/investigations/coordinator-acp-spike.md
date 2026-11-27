# Coordinator ACP spike: live results and gate verdict

- **Date:** 2026-11-25. The recorded conversations ran between 02:53 and 03:52 UTC on 2026-11-26.
- **Status:** Live spike complete. The acceptance gate passes, with the conditions under [Verdict](#verdict).
- **Related design:** [Native SwiftUI coordinator chat panel](../designs/native-swiftui-coordinator-chat-panel.md), "The spike: a validating study". That document is amended in the same change.
- **Repository baseline:** the recorded runs were built from `c16cd12a52e3` on `main`. The change was then rebased onto `e3dba375535f`.
- **Evidence:** 25 recorded conversations with the real adapter, the real `claude` CLI and the real Claude login, 2,061 journalled wire lines, eight captures of the app fixture, and the tests in `tools/boss/acp_client`. No recording is checked in.

Boss's proposed native coordinator needs structured conversation updates while keeping Claude Code's tools, instructions, login and background work. This is the validating study the design asked for: a probe held real conversations with the Claude ACP adapter, an isolated-only fixture in the app rendered them, and each criterion S1 to S16 was observed and is recorded here with the wire lines that show it.

## Verdict

**The gate passes. All eight required criteria (S1 to S8) were observed passing against the real system.** ACP v1 through the Claude adapter can carry the coordinator, and the design's backend choice stands.

Three of the passes hold only under conditions the design did not state. Each changes a later entry:

1. **S6 passes only with two adapter extensions.** With plain ACP v1 capabilities the adapter keeps `session/prompt` open until a background agent has finished and the model has answered it, so there is no turn that ends before the background work does. A message from another Claude session then produces an answer with no boundary at all. When the client advertises the adapter's `backgroundSubagents` and `asyncTasks` capabilities, the prompt ends at once and the model's later follow-up is bracketed by an explicit start and end. Those two signals are `session/update` kinds that are in neither ACP v1 nor the official Rust schema.
2. **S3 passes only when the model is set as a session option.** The model passed in `_meta.claudeCode.options` on `session/new` was overridden by the user's own configured model. `session/set_config_option` with the `model` option gave the coordinator's model.
3. **S2 passes only when the session directory is outside Claude Code's own temporary root.** In a session directory under `/private/tmp/claude-<uid>/`, Claude Code silently skips the directory's `CLAUDE.md`. The same happens with no adapter, so this is Claude Code's behaviour, not the adapter's.

The optional part of S3 was not obtained: three commands chosen to be harmless if they ran were all allowed by the auto-mode classifier, so no classifier denial was recorded. Nothing unsafe was tried to force one.

What this verdict does not cover. The engine service, the agent host, the handoff transport and restart durability are not in the spike, as the design says, and nothing here validates them. Whether running the coordinator through the headless path is acceptable under the account's plan terms is still the repo owner's decision; S8 shows only that it works and which usage window it reports.

## What was run

A probe binary, `boss-acp-probe` in the new `tools/boss/acp_client` crate, starts the adapter as a child process, speaks ACP v1 to it over stdio, and writes every line in both directions to a wire journal in the design's `{"seq", "ts_ms", "dir", "line"}` shape. It is driven by one JSON command per line and reports one JSON event per line. The app's fixture launches that binary by path and renders its events; it does not link the crate.

| Component          | Observed                                                                                                                                                                   |
| ------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Protocol           | ACP version 1, from the `initialize` response.                                                                                                                             |
| Adapter            | `@agentclientprotocol/claude-agent-acp` 0.110.0, reported in `agentInfo`. Installed with npm into a temporary directory and run with Node v24.8.0. It is not in the build. |
| Adapter's packages | `@agentclientprotocol/sdk` 2.9.1 and `@anthropic-ai/claude-agent-sdk` 0.3.330, read from the installed manifests.                                                          |
| Claude Code        | 2.1.332, the installed CLI, selected with `CLAUDE_CODE_EXECUTABLE` in every run but one. The adapter's own copy is 2.1.330 (S13).                                          |
| Rust schema        | `agent-client-protocol` 5.5.1, pinned exactly, which brings `agent-client-protocol-schema` 1.21.2.                                                                         |
| Model              | `claude-opus-5-5`, the coordinator's `opus`, reported by the adapter on every usage update.                                                                                |
| Engine             | An isolated engine built from this checkout and started by the recipe in `.claude/CLAUDE.md` on a socket under `/tmp`. The installed app was never launched.               |
| Machine            | Apple M2 Max, 64 GB, macOS 27.0.1. The app and the probe are Bazel `fastbuild` builds.                                                                                     |
| Installed size     | 423 MB of `node_modules`, of which the adapter's own Claude Code binary is 349 MB. The standalone release archives are 202.5 MB (arm64) and 213.5 MB (x64).                |

**The session directory** is rendered by the app's own code. `BossPaneModel.renderSessionDirectory` writes `CLAUDE.md` and `.claude/settings.local.json` into a directory the fixture names. It is the function the real coordinator's directory is written by, with the destination made a parameter. Scripted probe runs each use a fresh copy of a directory the fixture rendered. The production session directory was never read or written.

**The agent's environment** is rebuilt by the probe. Nothing starting with `BOSS_` or `CLAUDE_CODE_` is inherited, nor the parent's tmux pane. `BOSS_SOCKET_PATH` is set to the isolated engine's socket, and the built `boss` and `bossctl` are first on `PATH`. `ANTHROPIC_API_KEY` was not set in the environment the runs started from and is never passed on.

**Two capability profiles** were used, and each result below says which. Neither advertises `fs` or `terminal`, so the agent ran every tool itself.

- _Baseline:_ `clientCapabilities` with `fs` and `terminal` off and nothing else.
- _Extension:_ the same, plus `_meta.jetbrains.air` at version 1 with capabilities `backgroundSubagents` and `asyncTasks`.

**Excerpts** below are abridged from the journals. `→` is client to agent, `←` is agent to client, and the number is the journal `seq`. Session ids, the account's identity and home paths are removed.

## Results

| Criterion                      | Result                             | Observed                                                                                                                                                 |
| ------------------------------ | ---------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| S1: connection and three turns | Pass                               | `initialize` and `session/new` succeeded; three turns each streamed before their response.                                                               |
| S2: prompt, rules and memory   | Pass, with a condition             | The rendered prompt was in effect, a pre-cleared `boss` command ran with no request and reached the isolated engine, and the memory marker was read.     |
| S3: auto mode and model        | Pass; denial not obtained          | `auto` mode and `claude-opus-5-5`. The model had to be set as a session option. Three safe attempts provoked no classifier denial.                       |
| S4: allow and reject           | Pass                               | One request answered Yes and one answered No from the fixture, by the agent's own option ids.                                                            |
| S5: cancellation               | Pass                               | `session/cancel` ended a streaming turn and a turn waiting on a permission request; the next prompt was answered both times.                             |
| S6: background completion      | Pass with the extension profile    | Prompt ended first; background updates and the answer arrived with no prompt in flight; start and end signals correlated by task id. Not so in baseline. |
| S7: PNG paste                  | Pass                               | A PNG pasted into the fixture went out as an `image` block and the model described its shapes, colours and positions.                                    |
| S8: Claude plan login          | Pass                               | Every conversation ran with no API key, authenticated as a Claude plan account.                                                                          |
| S9: concurrent prompt          | Recorded: queued                   | A second prompt sent mid-turn waited; both got their own response, in order.                                                                             |
| S10: peer inbox                | Recorded: registered and delivered | The hosted CLI registers an inbox. A peer message produced an answer with no prompt in flight; the sender is named only after the answer.                |
| S11: Claude transcript         | Recorded: parsed                   | The transcript is where the engine expects it and `engine/transcript-markdown` reads it.                                                                 |
| S12: slash commands            | Recorded                           | `/compact` and `/context` work; `/help` and `/status` are answered "isn't available in this environment".                                                |
| S13: CLI version skew          | Recorded: no failure               | The installed 2.1.332 and the adapter's own 2.1.330 both started and answered.                                                                           |
| S14: rendering cost            | Recorded                           | 620 deltas at a median 47 ms apart; bringing the view up to date cost 17.6 ms per delta on average and grew with the answer's length.                    |
| S15: SDK recovery              | Recorded: Boss owns the id table   | The SDK's connection runs on an initialised stream and exposes its request ids, but drops the response to a request an earlier client sent.              |
| S16: child crash               | Recorded                           | The adapter survives, fails the prompt with an error, withdraws a pending permission request, and refuses further prompts until the session is resumed.  |

## Evidence for the required criteria

### S1: connection and three turns

Baseline profile. The handshake took 0.58 s from `initialize` to the answer to `session/set_mode`. Each of the three turns produced `agent_message_chunk` updates before its `session/prompt` response.

```text
 1 → {"id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false,…}}}
 2 ← {"id":1,"result":{"protocolVersion":1,"agentInfo":{"name":"@agentclientprotocol/claude-agent-acp","version":"0.110.0"},…}}
 3 → {"id":2,"method":"session/new","params":{"cwd":"…/session","mcpServers":[],"_meta":{"claudeCode":{"options":{…}}}}}
 8 ← {"id":2,"result":{"sessionId":"…","modes":{"currentModeId":"auto",…},"configOptions":[…]}}
15 → {"id":5,"method":"session/prompt",…}
21–29 ← agent_message_chunk ×9: "I'm The" … " I don't write code myself."
33 ← {"id":5,"result":{"stopReason":"end_turn",…}}
34 → {"id":6,"method":"session/prompt",…}        43–46 ← agent_message_chunk ×4        48 ← {"id":6,"result":{"stopReason":"end_turn",…}}
49 → {"id":7,"method":"session/prompt",…}        57–63 ← agent_message_chunk ×7        65 ← {"id":7,"result":{"stopReason":"end_turn",…}}
```

Across the scripted runs 32 turns ended `end_turn`, the only other stop reason seen was `cancelled`, and no line failed to parse as JSON-RPC.

### S2: prompt, rules and memory

Baseline profile, `auto` mode, and again in `default` mode for the allow rules.

**The rendered prompt is in effect.** Asked for its role in one sentence, the session answered "I'm The Boss, the single coordinating session in Boss V2: I coordinate work and hand it to workers through `bossctl` and `boss`, and I don't write code myself." Claude Code's own transcript for that session lists the instruction files it loaded: the user's `~/.claude/CLAUDE.md`, the session directory's `CLAUDE.md` (52,687 characters, type `Project`), and the memory index.

**The allow rules are in effect.** In `default` mode, where anything not pre-cleared asks first, one prompt asked for a `boss` command and then for `touch`. The `boss` command ran with no request; `touch` produced one. The same conversation is the first S4 capture.

```text
15 → session/prompt "… First run `boss product list --json --no-input`. Then run `touch permission-probe-allow.txt` …"
   ← tool_call "boss product list --json --no-input" … status completed      (no session/request_permission)
32 ← {"id":0,"method":"session/request_permission","params":{"toolCall":{"title":"touch permission-probe-allow.txt",…},"options":[…]}}
```

**`boss` reaches the isolated engine.** A product named "ACP spike isolated engine marker 7c41" was created in the isolated engine only. The command's output, as the model reported it and as the tool row shows it, was exactly that one product.

**The memory marker is read.** A marker note was placed in `~/.claude/projects/-private-tmp-boss-acp-spike-…-session/memory/`, the directory Claude Code derives from the session directory, with a one-line index entry. Asked for the marker, the session read that file with its `Read` tool (journal `seq` 51 to 55) and answered `HERON-4417-QUARTZ`, the value in the note.

**The condition.** The first attempt failed. With the session directory under `/private/tmp/claude-<uid>/…`, the answer to the role question was the user's global instructions and the context was 24,637 tokens where the passing run's was 42,604. Claude Code's transcript listed only the user file and the memory index. The same question asked of `claude -p` directly, with no adapter, gave the same result in that directory and the correct one in a directory under `/tmp`. So Claude Code does not load a project `CLAUDE.md` from inside its own temporary root, with or without the adapter. Every later run used a session directory under `/tmp`.

### S3: auto mode and model

**Mode.** `session/new` reported `currentModeId: "auto"` and `session/set_mode` with `auto` succeeded. The CLI the adapter started carries `--permission-mode=auto`. In the S4 conversation the same call with `default` took effect, so the call is what selects the mode.

**Model.** A session created with `_meta.claudeCode.options.model` set to `opus` and nothing else ran on another model: the adapter resolves the starting model from the `ANTHROPIC_MODEL` variable and then from the user's settings, and its log shows it setting that model after the session started. Setting the session's `model` option worked:

```text
 9 → {"id":3,"method":"session/set_config_option","params":{"sessionId":"…","configId":"model","value":"opus"}}
11 ← {"id":3,"result":{"configOptions":[{"id":"model","currentValue":"opus",…},…]}}
20 ← usage_update … "_meta":{"_claude/model":"claude-opus-5-5"}
```

**Inline settings.** `_meta.claudeCode.options.settings` carrying `{"crossSessionInbound":"accept"}` reached the CLI as `--settings {"crossSessionInbound":"accept"}` on its command line, read from the running process.

**Classifier denial: not obtained.** In `auto` mode the session was asked to run, one per turn, `git push --force origin main` (in a directory that is not a repository), `sudo -n true`, and `rm -rf` of a path that does not exist. Each was chosen so that running it changes nothing. All three ran: the classifier allowed each one and the failures came from `git` and `sudo` themselves. No riskier command was tried. The nearest structured shape seen is on tool calls that were refused or aborted, which carry `_meta.claudeCode.nonExecutionKind` (value `permission-rule`) on the failed `tool_call_update`.

### S4: allow and reject

Baseline profile, `default` mode, driven from the app fixture. Both answers were sent by the fixture's own button action, with the option id the agent offered.

```text
32 ← {"id":0,"method":"session/request_permission","params":{"toolCall":{"toolCallId":"toolu_…","title":"touch permission-probe-allow.txt","kind":"execute",…},
      "options":[{"optionId":"allow-once","name":"Yes","kind":"allow_once"},{"optionId":"reject","name":"No","kind":"reject_once"}]}}
34 → {"id":0,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}}
71 ← {"id":5,"result":{"stopReason":"end_turn",…}}
77 ← {"id":1,"method":"session/request_permission",… "title":"touch permission-probe-reject.txt" …}
79 → {"id":1,"result":{"outcome":{"outcome":"selected","optionId":"reject"}}}
   ← tool_call_update status failed "User refused permission to run tool"
92 ← {"id":6,"result":{"stopReason":"end_turn",…}}
```

Afterwards `permission-probe-allow.txt` existed in the session directory and `permission-probe-reject.txt` did not. The agent numbers its own requests from 0, independently of the client's ids: its request 1 at `seq` 77 has nothing to do with the client's request 1, the `initialize`. A response is matched by direction as well as by id. The adapter offered exactly two options, with no "always allow".

### S5: cancellation

Two cases, both followed by a prompt that was answered.

A streaming turn, stopped from the fixture's Stop button. The prompt's response arrived 6 ms after the cancel:

```text
 93 → {"id":7,"method":"session/prompt",… "Write a 600-word essay about lighthouses." …}
122 → {"method":"session/cancel","params":{"sessionId":"…"}}
124 ← {"id":7,"result":{"stopReason":"cancelled",…}}
125 → {"id":8,"method":"session/prompt",…}        129 ← {"id":8,"result":{"stopReason":"end_turn",…}}
```

A turn waiting on a permission request. The probe answers every pending request `cancelled` before it sends the cancel, as the protocol requires:

```text
24 ← {"id":0,"method":"session/request_permission",… "touch cancel-probe.txt" …}
27 → {"id":0,"result":{"outcome":{"outcome":"cancelled"}}}
28 → {"method":"session/cancel",…}
29 ← tool_call_update status failed "Tool permission request failed: Error: Tool use aborted"
31 ← {"id":5,"result":{"stopReason":"cancelled",…}}
32 → {"id":6,"method":"session/prompt",…}        36 ← {"id":6,"result":{"stopReason":"end_turn",…}}
```

The file was not created.

### S6: background completion

The prompt asked for a background agent that sleeps 20 seconds and returns a marker, for the single word "launched" straight away, and for a sentence quoting the marker once the agent finished.

**Baseline profile: the prompt is held open.** "launched" arrived, then nothing ended. The background agent's tool calls, the model's follow-up answer and only then the prompt's response arrived, 24.4 s after "launched". The whole exchange is one prompt turn, and there is no update with no prompt in flight except one trailing `usage_update`.

```text
30 ← agent_message_chunk "launched"
32–37 ← tool_call "sleep 20" and "SubagentHandback", each with _meta.claudeCode.parentToolUseId
39–41 ← agent_message_chunk "The background agent finished and returned \"BACKGROUND-DONE-58\"."
44 ← {"id":5,"result":{"stopReason":"end_turn",…}}
```

**Extension profile: the prompt ends, and the follow-up has a start and an end.**

```text
 1 → initialize … "clientCapabilities":{…,"_meta":{"jetbrains":{"air":{"version":1,"capabilities":["backgroundSubagents","asyncTasks"]}}}}
 2 ← … "_meta":{"jetbrains":{"air":{"version":1,"capabilities":[…,"asyncTasks",…,"backgroundSubagents"]}}}
30 ← agent_message_chunk "launched"
32 ← {"id":5,"result":{"stopReason":"end_turn",…}}                                   (18 ms after "launched")
33–36 ← tool_call "sleep 20", "SubagentHandback" with parentToolUseId                 (no prompt in flight, no turn open)
37 ← {"sessionUpdate":"async_task_spawned","asyncTaskId":"autonomous-turn-7126…","taskType":"turn","name":"Claude is working","canStop":true}
39–42 ← agent_message_chunk "The background agent finished and returned \"BACKGROUND-DONE-58\"."
44 ← {"sessionUpdate":"async_task_state_update","asyncTaskId":"autonomous-turn-7126…","state":"completed"}
45 ← usage_update … "_claude/origin":{"kind":"peer","handback":true,…}
```

The start is `async_task_spawned` with `taskType: "turn"`. The end is `async_task_state_update` with the same `asyncTaskId` and a finished state. The same pair bracketed the follow-up in four separate conversations: the run above, the same prompt driven from the app fixture, a background shell command (`sleep 15; echo SHELL-DONE-91` with `run_in_background`), and a message from a peer session (S10). The background command itself was also a task, `taskType: "shell"`, spawned and completed around the command.

Two details matter to a client:

- **Background work is not a turn.** The background agent's own tool calls (`seq` 33 to 36) arrive before `async_task_spawned`, with no prompt in flight and no turn open. Only the `turn` task says the model is working. An update outside a prompt must not open a turn on its own.
- **The official Rust schema cannot read these updates.** `SessionNotification` fails to parse them with "unknown variant `async_task_spawned`". The fields have to be read from the raw line. The adapter's finished states are `completed`, `failed` and `stopped`.

### S7: PNG paste

Baseline profile, `auto` mode, from the app fixture. A 480 by 320 PNG with a red circle, a yellow square and a green bar holding three white rectangles on a blue background was put on a pasteboard and taken by the composer's paste, which wrote it to a file byte for byte (1,740 bytes). Send put it on the wire as an `image` block:

```text
159 → {"id":7,"method":"session/prompt","params":{"prompt":[{"type":"image","mimeType":"image/png","data":"<2,320 base64 characters>"},{"type":"text","text":"Describe the attached image: …"}]}}
```

The answer listed a red circle upper left, a yellow square upper right, a green horizontal bar near the bottom and three small white rectangles evenly spaced inside it, on a solid blue background. The agent's `promptCapabilities` report `image: true`.

The paste ran against a pasteboard of the fixture's own, not the general one, so that a capture run neither reads nor replaces the clipboard of whoever is using the machine. The keyboard gesture itself was not exercised.

### S8: Claude plan login

Every conversation ran with `ANTHROPIC_API_KEY` absent. The probe reports what it did with the inherited environment: the key was not set and not passed on, and there were no other `ANTHROPIC_` or Bedrock variables. The adapter reported the route itself:

```text
 2 ← initialize result … "authMethods":[]
 5 ← {"method":"auth/status_update","params":{"state":"authenticated","accounts":[{"active":true,…,"_meta":{"claudeCode":{"kind":"account","plan":"…"}}}]}}
31 ← usage_update … "_claude/rateLimit":{"status":"allowed","rateLimitType":"seven_day","isUsingOverage":false,…}
```

The adapter's log line for each session reads `apiType=native baseUrl=native`. The rate-limit record names the plan's seven-day window, which is the evidence that these sessions drew on the plan's limits. That is an observation of today's behaviour, not a statement about policy.

## Evidence for the recorded criteria

### S9: a second prompt mid-turn

Baseline profile. A second `session/prompt` was sent 2.5 s after the first, before the first had streamed anything. It was queued. The first turn ran to its end, then the second ran, and each request got its own response in order. The agent advertises `promptQueueing: true`.

```text
 15 → {"id":5,"method":"session/prompt",… "Count from 1 to 40 …"}
 20 → {"id":6,"method":"session/prompt",… "Reply with exactly the word: second"}
 22–151 ← agent_message_chunk … "40 — Forty winks make a nap."
155 ← {"id":5,"result":{"stopReason":"end_turn",…}}
157 ← agent_message_chunk "second"
159 ← {"id":6,"result":{"stopReason":"end_turn",…}}
```

Nothing in the update stream marks where the second turn's updates begin; the boundary is the first prompt's response. The engine's own single-flight queue stays the plan: it is what lets Stop return unsent messages to the composer.

### S10: the cross-session inbox

The hosted CLI registers an inbox in every configuration tried: `~/.claude/sessions/<pid>.json` exists for the adapter's Claude process with a messaging socket, `kind: "interactive"`, `entrypoint: "sdk-ts"` and `peerProtocol: 1`. It registered with and without the `crossSessionInbound` setting, and on this machine a peer message was accepted in both cases.

A message sent from another local Claude Code session to the fixture session was answered. In the baseline profile the answer simply appears with no prompt in flight, and the only record of where it came from follows the answer:

```text
24 ← {"id":5,"result":{"stopReason":"end_turn",…}}
26–28 ← agent_message_chunk "PEER-RECEIVED-63"
30 ← usage_update … "_claude/origin":{"kind":"peer","name":"<sending session>","verifiedLocal":true,"body":"<the message text>",…}
```

In the extension profile the same answer is bracketed like any other agent turn, with the description "Claude Code started a turn without a prompt." The origin still arrives after the end. The peer's text is never sent as a message of its own.

One side effect was found and fixed in the probe. Claude Code records the tmux pane it believes it is in when it registers. A session started from a worker's shell registered itself under that worker's pane until the probe stopped passing `TMUX` and `TMUX_PANE` on.

### S11: Claude Code's transcript

The transcript for a session appears at `~/.claude/projects/<session directory with every non-alphanumeric character replaced by "-">/<session id>.jsonl`, which is the directory the engine derives. `boss-acp-probe transcript-check` parsed the three-turn session's transcript with `parse_transcript_checked`: 63 lines, none unreadable, schema recognised, 10 events (3 user texts, 3 assistant texts, 2 thinking blocks, 2 tool uses).

The tool results in that transcript are `tool_result` blocks inside `user` records, as in any Claude Code transcript, and the parser does not turn those into events. That is how it reads terminal sessions too; it is not specific to the adapter. The transcript holds thinking blocks that never appeared on the wire: no `agent_thought_chunk` was seen in any run.

### S12: slash commands

Baseline profile, each typed as a prompt.

| Prompt     | What came back                                                                                                                                                                                                                                 |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `/help`    | One `agent_message_chunk`, "/help isn't available in this environment.", then `end_turn` with zero usage, 21 ms after the prompt.                                                                                                              |
| `/status`  | The same, with "/status".                                                                                                                                                                                                                      |
| `/context` | The context usage table as an `agent_message_chunk`, sent twice as two messages with different ids, then `end_turn`.                                                                                                                           |
| `/compact` | A `tool_call` titled "Compact conversation" (`toolName: "compact"`), `in_progress` then `completed` with `rawOutput` `{"trigger":"manual","preTokens":42602,"postTokens":7073,"durationMs":9451}`, a `usage_update` of 7,073, then `end_turn`. |

The prompt after `/compact` was answered normally. Compaction is a tool call here, not one of the schema's compaction update kinds. The adapter also publishes the commands it does support in `available_commands_update`, up to 71 of them in these sessions; `help` and `status` are not among them.

### S13: installed CLI or the adapter's own

The same one-turn conversation ran once with `CLAUDE_CODE_EXECUTABLE` naming the installed CLI (2.1.332) and once without it, where the adapter ran its own copy (2.1.330, from `@anthropic-ai/claude-agent-sdk-darwin-arm64`). Both started, answered and exited with status 0, and the adapter's log was the same apart from timings (session creation 267 ms and 517 ms). The process list confirmed which binary ran in each case. No startup failure was seen at this two-patch skew.

The installed CLI's command line, read in full from the running process, carries `--disallowedTools=AskUserQuestion` and `--allow-dangerously-skip-permissions`. The first means the hosted coordinator cannot ask a structured question; the second is why `bypassPermissions` is among the modes the session offers.

### S14: rendering cost

One long answer from the app fixture: 9,413 characters with three code blocks, a table and lists, delivered as 620 deltas over 30.9 s. The deltas arrived a median 47 ms apart (95th percentile 77 ms), about 20 a second, averaging 15 characters.

After each delta the fixture applied it, brought the SwiftUI hierarchy up to date, and drew the window into a bitmap, timing each step on the main thread.

| Step                                 | Mean    | Median  | 95th percentile | Maximum |
| ------------------------------------ | ------- | ------- | --------------- | ------- |
| Apply the event to the model         | 0.06 ms | 0.03 ms | 0.05 ms         | 2.2 ms  |
| Update the view, re-parsing markdown | 17.6 ms | 14.2 ms | 44.8 ms         | 65.4 ms |
| Draw the whole window                | 64.7 ms | 64.3 ms | 73.9 ms         | 84.1 ms |

The update step grows with the answer, because the fixture re-parses the whole streaming message on each delta: 5.5 ms on average below 1,000 characters, 9.2 ms up to 2,500, 14.5 ms up to 4,500 and 24.2 ms beyond. The draw is a full-window render at 2x into a bitmap, which is more than an on-screen incremental redraw costs, so read it as an upper bound.

What this sets. Rendering every delta does not keep up: the update step alone used 10.9 s of the 30.9 s stream, and with the draw the work exceeds the stream's length. A publish interval of 100 to 150 ms brings a 20-per-second stream down to 7 to 10 updates a second. The growth with length is the stronger result: the streaming tail must not be re-parsed together with the blocks that are already complete, which is the split the design already describes. These are unoptimised-build numbers from one machine and one answer.

### S15: the official crate's connection on a live stream

Two questions, answered with the SDK's own v1 client connection against the real adapter (`boss-acp-probe sdk-attach`) and again in tests against a scripted peer.

An earlier client wrote `initialize`, `session/new`, the option and mode calls and one prompt as plain lines with ids 1 to 5, and stopped without reading the prompt's response. The SDK connection was then given the same stream.

```text
15 → {"id":5,"method":"session/prompt",… "Reply with exactly the word: first"}                  (earlier client; never reads the answer)
16 → {"id":"10d0a7f3-ae5a-4254-840c-77839d471d2d","method":"session/prompt",… "… second"}       (first line the SDK connection wrote)
25 ← {"id":5,"result":{"stopReason":"end_turn",…}}
29 ← {"id":"10d0a7f3-…","result":{"stopReason":"end_turn",…}}
```

- **It runs on an initialised stream and exposes request ids.** The first line it wrote was the prompt, not an `initialize`, and the id on the wire is the one `SentRequest::id()` reported. `prepare_request` gives the id before anything is sent.
- **It cannot recover an earlier client's request.** Two responses arrived after the handover. The connection's dispatch handler, which sees every message it delivers, saw one: its own. The response to the earlier prompt was dropped, and nothing in the public API registers an id to wait for. Its ids are generated UUIDs the caller cannot choose.

So Boss keeps its own line framing and request-id table and takes the typed messages from the official crate. `boss_acp_client::PreparedConnection::expect_response` registers an id from the journal before the connection reads its first byte, so a response that is already buffered in the stream is taken by its waiter, not read first as an unknown response. It takes the response when it comes and sends nothing twice. An id that is already waiting is refused, and a new request skips any id that is. The connection also reports each response's arrival on the same ordered stream as the agent's notifications, which is what lets the probe report a turn's end between the updates of that turn and the next one. That path is tested against a scripted peer together with a permission answer, a new request, a response buffered before the connection starts, and updates and responses that arrive in one read. It was not run against the live adapter, because attaching a second client process to a live stream needs the agent host, which is not in the spike.

**Later run against the live adapter, through the agent host (2026-11-27).** The engine chat service (`engine/coordinator-chat`) takes no response through the SDK's connection: it reads the host's journal records, so a response to a request an earlier engine process sent is reduced like any other record, and the request's id is known from the journal. `live_adapter_probe` restarted the service twice against the real adapter 0.110.0, Claude Code 2.1.332 and the real login, on the `haiku` model. First, with a prompt in flight (client request 5) and a second queued: the new service saw the response to request 5, sent the queued message as request 6 and saw its response. Second, with a permission request pending: the agent's request was id 0 (journal record 35) while the client's own ids had reached 7; the new service recovered it, answered it as id 0 in the client-to-agent direction (record 37), and the allowed `touch` ran. The harness checks that the first turn had not ended before the restart.

### S16: the Claude child is killed

The adapter's Claude Code child was sent `SIGKILL` part-way through a streamed answer.

```text
36 ← {"id":5,"error":{"code":-32603,"message":"Internal error: The Claude Agent process exited unexpectedly. Please start a new session."}}
37 → {"id":6,"method":"session/prompt",…}
38 ← {"id":6,"error":{"code":-32002,"message":"Resource not found","data":{}}}
```

- **The adapter survives.** It stayed up with no children, logged "Claude Agent process died: Claude Code process terminated by signal SIGKILL", and exited with status 0 only when its input closed.
- **The turn in flight fails** with error `-32603`, and every later prompt on that session fails at once with `-32002`. The adapter does not restart the CLI by itself.
- **A pending permission request is withdrawn.** Killed while a request was waiting, the adapter sent `{"method":"$/cancel_request","params":{"requestId":0}}` before failing the prompt. An answer sent to that request afterwards was accepted silently and did nothing.
- **The session can be resumed on the same adapter.** `session/resume` with the same session id succeeded, the adapter started a new CLI with `--resume=<session id>`, and the next prompt recalled a word from before the kill. `session/load` also succeeded and replays the history as `user_message_chunk` and `agent_message_chunk` updates.

The consequence for the design: a dead pane is not the only failure. A dead Claude Code under a live adapter leaves the pane alive and the session dead until something resumes it.

## Source findings, checked against the live system

The earlier source review of the pinned adapter and SDK made four claims. Each was tested.

| Source finding                                                                                                       | Live result                                                                                                                                                                 |
| -------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Without `backgroundSubagents` the adapter can hold `session/prompt` open while a background subagent runs.           | Confirmed. The prompt's response came 24.4 s after the model's first answer, after the background agent and the follow-up (S6).                                             |
| With `asyncTasks`, the adapter reports an autonomous turn as a task with `taskType: "turn"` that completes or fails. | Confirmed, in four conversations (S6, S10). Also found: the official Rust schema cannot parse those updates.                                                                |
| The Rust SDK exposes request ids, but a fresh connection drops responses to requests it did not send.                | Confirmed live and in tests (S15).                                                                                                                                          |
| An explicitly selected CLI receives its settings on the command line; the model and mode may not be what was asked.  | Confirmed for settings. Refined for the model: the option on `session/new` is not enough, the session option is (S3). `auto` mode was honoured for the coordinator's model. |

The review's isolation concern, that a non-production engine and an app pointed at one both used the fixed production session directory, was closed on `main` by mono#3920 while this study ran. The recorded runs were made on the earlier baseline and did not depend on it: the fixture renders into the directory it is given and the probe sets the engine socket itself. After the rebase onto that change the fixture was run once more and behaved the same.

## What else the live runs showed

- **The adapter exits when its input closes, with status 0.** A pipe to a child closes only when its handle is dropped. Shutting the writer down without dropping it left the adapter waiting until it was killed; `Connection::shutdown_writer` now drops it, with a test that uses a real child process.
- **Updates that carry no turn.** `available_commands_update` (27,587 bytes, the largest line recorded), `config_option_update`, `session_info_update` (the session's generated title) and the adapter's `auth/status_update` and `_auth/status_update` notifications arrive around and between turns. The account notifications carry the signed-in identity; the probe's events drop it.
- **Who started a turn is reported at its end.** The last `usage_update` of a turn carries `_meta._claude/origin` with `kind` `human`, `peer` or `task-notification`. It also carries a cost figure and, during a turn, the rate-limit record.
- **Tool output is markdown.** A command's output arrives as a text block already wrapped in a code fence. The structured result is in `_meta.claudeCode.toolResponse` on the update. A nested call carries its parent in `_meta.claudeCode.parentToolUseId`.
- **Prompts are not echoed.** No `user_message_chunk` was seen in a live turn. They appear only when `session/load` replays history.
- **Background output lands in Claude Code's temporary root.** A background command's output file was written under `/private/tmp/claude-<uid>/<encoded session directory>/<session id>/tasks/`.

## What the spike does not show

- Nothing about the engine service, the agent host, the journal replay handshake, the intent log or handoff delivery. They are not in the spike.
- Attaching a second client process to a live adapter stream. The id-table recovery is tested against a scripted peer only.
- A classifier denial's shape (S3).
- The paste keyboard gesture, and any behaviour of a window that is on screen. The captures are of a window that is never presented.
- Long sessions: no automatic compaction was reached, no session held more than a few turns, and the longest ran for about three minutes.
- Version skew larger than two patch releases (S13), and any adapter release other than 0.110.0.

## Reproducing

The probe and the fixture are in the tree; the adapter is installed by hand. The session directory must be outside `/private/tmp/claude-<uid>/`.

```sh
npm install --prefix "$ADAPTER_DIR" @agentclientprotocol/claude-agent-acp@0.110.0
bazel build //tools/boss/acp_client:boss-acp-probe //tools/boss/cli:boss //tools/boss/bossctl:bossctl
env -u BOSS_EVENTS_SOCKET bazel run //tools/boss/engine/core:engine -- --socket-path "$ENGINE_SOCKET"

# One scripted conversation; commands are JSON lines such as {"op":"prompt","text":"…"}.
boss-acp-probe converse --cwd "$SESSION_DIR" --journal "$RUN/journal.jsonl" --script "$RUN/script.jsonl" \
  --boss-socket "$ENGINE_SOCKET" --path-prefix "$BUILT_BIN" --claude-executable "$(command -v claude)" \
  --setting-source user --setting-source project --setting-source local \
  --config model=opus --mode auto --settings-json '{"crossSessionInbound":"accept"}' \
  -- node "$ADAPTER_DIR/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js"

# The same conversation through the app fixture, captured without showing a window.
BOSS_SOCKET_PATH=/tmp/boss-shot-$(uuidgen).sock BOSS_ENGINE_AUTOSTART=0 \
  bazel run //tools/boss/app-macos:Boss -- --capture-reveal-state=coordinator-chat-probe --capture-to "$RUN/final.png" \
  --capture-chat-probe="$BUILT_BIN/boss-acp-probe" --capture-chat-session="$SESSION_DIR" \
  --capture-chat-journal="$RUN/journal.jsonl" --capture-chat-script="$RUN/steps.json" \
  --capture-chat-probe-arg=--boss-socket --capture-chat-probe-arg="$ENGINE_SOCKET" …
```

Add `--air-capability backgroundSubagents --air-capability asyncTasks` for the extension profile. `boss-acp-probe sdk-attach` takes the same session flags and runs the S15 check; `boss-acp-probe transcript-check --cwd … --session …` runs S11. The app's `BOSS_SOCKET_PATH` names a socket nothing listens on, so the capture instance registers with no engine; the isolated engine is reached only by the probe's `--boss-socket`.

The eight captures taken for this study are registered with the run that produced them: a part-streamed answer, a tool row with its result, an unresolved permission card, the same conversation after Yes and No, a pasted image with its description, Stop followed by a new prompt, a background agent's completion with the agent's turn bracketed, and the end of the long answer used for S14.
