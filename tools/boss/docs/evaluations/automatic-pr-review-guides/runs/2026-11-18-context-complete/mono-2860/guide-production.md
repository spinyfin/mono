# Coordinator handoff across restarts — PR #2860

## Orientation

After a restart, Boss could forget facts the user had already explained in chat: it filed work against a build-and-test host that had been deliberately shut down, then briefed another agent using an operational setting the user had already reversed. This change adds a persistent handoff so a replacement session receives those facts immediately, or an explicit warning that the handoff is missing, old, or unreadable.

The **coordinator** is the Claude Code conversation through which the user directs Boss; the **engine** is the background service that manages its lifetime. A **handoff** is a short, continually updated note of facts, decisions, unfinished conversations, and prohibitions. This capability spans the coordinator instructions in the macOS app, the Boss command-line interface, and the engine’s session-start logic. The coordinator is instructed to save the note during normal conversation; the engine supplies it as the first prompt of a fresh session.

**tmux** is the terminal-session host that can keep the coordinator running across an engine restart. An **adopted session** is that surviving conversation, which retains its context and receives no new handoff prompt. A **spawn token** identifies one coordinator session; comparing tokens lets the engine distinguish a note attributed to the outgoing session from one left by an earlier session.

## Change overview

The new capability addresses context loss when Boss creates a replacement coordinator. Previously, [the launch command](https://github.com/spinyfin/mono/blob/91567c86a7195ee78f3e340fbcf6c5bccc5ebedc/tools/boss/engine/core/src/coordinator_tmux.rs#L708-L740) started Claude without a handoff prompt. Now every launch through [`start_new`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L699-L743) prepares one.

The coordinator can replace the saved note with `boss handoff write <file>` or `boss handoff write -` for standard input, and retrieve it with `boss handoff show`. Both support the existing structured-output mechanism. The [new prompt section](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift#L440-L464) instructs the coordinator to rewrite the note whenever relevant facts, decisions, prohibitions, or open threads change, and before acknowledging a restart request.

That refresh cadence is a model instruction, not an engine-triggered snapshot. Likewise, summarizing the handoff and checking facts before acting are prompt instructions. The engine enforces text validation, persistence, attribution, and initial-prompt construction.

## Core implementation

The implementation has three connected parts:

- **Write and read:** the [command-line handler](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/handoff.rs#L47-L117) sends requests to the engine. These are RPCs—request-and-response messages between the command and the background service. The [engine handlers](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L28-L130) validate writes, stamp them, return results, and distinguish absent data from read errors.
- **Storage and composition:** [`coordinator_handoff.rs`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L168-L243) stores one JSON record and builds the incoming session’s brief. It reuses the [existing metadata store](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/workitems.rs#L1531-L1554), rather than adding a database table or repository file.
- **Delivery:** the [existing coordinator lifecycle](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L333-L383) passes a start reason into fresh-session creation. Adoption continues through the existing prompt-change nudge, a request to reread changed instructions.

## Implementation walkthrough

### 1. Save the note while the conversation is alive

The [write handler](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L79-L119) first calls `validate_handoff_body`, then reads the currently recorded coordinator token and stamps the write with engine time. An absent coordinator record produces an empty writer token. Attribution therefore comes from the engine’s current record, not a caller-supplied writer identity.

[`validate_handoff_body`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L205-L224) trims surrounding whitespace, rejects blank text, and caps the trimmed UTF-8 body at 16 KiB. A successful write replaces the whole record under `coordinator.handoff`; it does not append a transcript. The successful-write audit event records byte count, writer token, and time.

The app adds the commands to its [automatic-mode permission list](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift#L201-L216). Separately, the [worker policy](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/worker-policy/src/policy.rs#L431-L446) classifies both requests as `CoordinatorOnly`. The existing [trust gate](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/trust.rs#L270-L292) applies that restriction to worker-classified connections when `worker_rpc_tier` is enabled; non-worker terminals remain outside that gate.

### 2. Preserve the outgoing identity before recording the replacement

The important ordering in [`start_new`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L699-L714) is:

```rust
let previous = work_db
    .coordinator_tmux_record()?
    .map(|record| PreviousSession::from(&record));
```

Only after this capture does the engine generate and persist the replacement token. Otherwise, the brief would compare the saved writer against the incoming session instead of the outgoing one.

The durable coordinator record now includes [`spawned_at`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/coordinator_tmux.rs#L64-L108), stamped in the same transaction as the intended launch. Older or unparseable timestamps read as unknown. This timestamp supplies explanatory context; token equality determines writer attribution.

[`CoordinatorSpawn`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L69-L98) groups shared lifecycle dependencies. It retains separate handles for inspecting the existing terminal server and creating sessions on the intended server. First creation, missing sessions, exited processes, and confirmed reset/model replacement all converge on `start_new` with an explicit reason. A live matching-token session instead follows [adoption](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L333-L383).

### 3. Compose an honest account of what survived

The storage reader returns `HandoffState::Present`, `Missing`, or `Unreadable`; database and JSON decoding failures become `Unreadable`. The [brief composer](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L282-L346) refines a present record using:

```rust
let by_previous =
    previous.is_some_and(|p| !p.spawn_token.is_empty() && p.spawn_token == handoff.writer_spawn_token);
```

| Relevant state                                            | Brief reports                                                          |
| --------------------------------------------------------- | ---------------------------------------------------------------------- |
| Nonempty writer matches the outgoing token                | `HANDOFF PRESENT`, attributed to the session that just ended           |
| Known writer differs, with an outgoing record available   | `HANDOFF STALE`, retaining the older body and warning about the gap    |
| Empty writer token, or no outgoing record to compare      | `HANDOFF PRESENT (writer unknown)`                                     |
| No stored note on first creation, with no previous record | `NO HANDOFF: none is expected`                                         |
| No stored note otherwise                                  | `NO HANDOFF AVAILABLE`, explicitly warning that chat facts may be lost |
| Stored value cannot be read                               | `HANDOFF UNREADABLE`, including the error                              |

“Stale” is an attribution result, not an elapsed-time cutoff. The brief separately prints write time and, when known, previous-session start time. Its [closing instructions](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L349-L368) ask the coordinator to explain the state, confirm operational facts, and refresh the note. A transcript directory is mentioned only when it exists, as a last resort.

### 4. Deliver the brief through the initial prompt

[`prepare_session_start_brief`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L782-L849) writes the composed text to `.claude/handoff-brief.txt` and supplies its contents as Claude’s positional initial prompt. The resulting command has this shape, with the session path shown illustratively:

```sh
exec claude --model opus --permission-mode auto "$(cat '/session/.claude/handoff-brief.txt')"
```

If writing that file fails, the function supplies a quoted inline prompt naming the error and directing the coordinator to `boss handoff show`. It also records `brief_unwritable`; normal brief preparation records the handoff state, start reason, and attribution.

The [show handler](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L45-L68) returns no value only for missing storage, and an error for unreadable storage. The [command-line rendering](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/handoff.rs#L81-L115) uses the shared elapsed-time formatter and the existing local-time display convention.

### Worked example: a host shutdown survives a process exit

The [composer test fixture](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L407-L431) contains “greyarea is shut down” and “tmux hosting re-enabled.” In the [present-handoff test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L488-L504), the note was written four minutes ago by `token-a`; the outgoing session also has `token-a` and started an hour ago.

1. **Before:** an exited Claude process led to a fresh launch whose [command ended at `--permission-mode auto`](https://github.com/spinyfin/mono/blob/91567c86a7195ee78f3e340fbcf6c5bccc5ebedc/tools/boss/engine/core/src/coordinator_tmux.rs#L728-L740). No saved chat brief accompanied it.
2. **Now:** the lifecycle captures the outgoing token, records a new token, and compares the saved writer against the captured one. The brief says `HANDOFF PRESENT: written by the session that just ended`, includes “4 minutes ago,” names the process exit, and includes the host-shutdown fact.
3. **Observable result:** the replacement receives that information in its initial prompt, with instructions to summarize it and confirm current facts before acting. The [dead-pane lifecycle test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2123-L2159) asserts a new token and launch, the shutdown text, and absence of a stale warning.

The contrasting [stale test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L506-L518) leaves a three-day-old note from `token-a` while the outgoing session is `token-b`, started two hours ago. The same body is preserved, but the brief explicitly reports `HANDOFF STALE` and that the ended session never wrote a handoff.

## Tests and test infrastructure

**Added tests:**

- [Storage, validation, and composition tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L434-L594) cover trimming, blank rejection, the exact size boundary, missing/present/corrupt storage, exact nonempty-token matching, present versus stale attribution, unknown previous writer, first creation, missing and unreadable warnings, transcript-path encoding, and writing the brief file.
- [Lifecycle tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2089-L2243) assert the initial-prompt command suffix and brief contents for first creation, process exit, missing sessions, older handoffs, and reset with corrupt storage.
- [Handler tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs#L81-L147) cover empty reads, write/read attribution across session replacement, whole-note replacement, rejection without changing the previous note, and corruption returning an error.

**Modified tests and fixtures:** existing lifecycle tests adopt the new [`spawn_ctx` helper](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L1208-L1259); new helpers extract the recorded launch command and read the brief. The handler suite uses a [temporary database and response sink](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs#L11-L58), asserting exactly one response per call. The [metadata test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/coordinator_tmux.rs#L164-L183) additionally checks that launch intent stamps a positive start time.

The existing [worker-denial test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/worker-policy/src/tests.rs#L324-L334) adds both requests. The event fixtures add [both handoff response variants](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/protocol/src/wire/frontend_event_wire_tests.rs#L1455-L1468) to the existing [serialization-tag and structural round-trip tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/protocol/src/wire/frontend_event_wire_tests.rs#L2147-L2176). No tests are removed, and no build/configuration files change.

## Evidence

- The author ran `bazel test //tools/boss/... -//tools/boss/app-macos/... -//tools/boss/installer/...`; result: 94 tests pass, with 5,014 passed in the engine core suite, including 24 new tests reported as covering validation, the three states, stale-versus-present attribution, each start reason, launch command shape, and the RPC round trip.
- The author ran `bazel test //tools/boss/app-macos/...`; result: 8 pass, including compilation and passing checks for the Swift prompt literal and permission-list changes.
- The author ran `checkleft run`; result: no findings.
- The author reported that a live coordinator restart against a real `claude` binary was not exercised because a worker may not launch a production engine, and reported that the launch-command test pins the worker-proven command shape.

No screenshots are registered.
