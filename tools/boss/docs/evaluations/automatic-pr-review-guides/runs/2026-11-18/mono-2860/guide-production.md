# Coordinator session handoff across restarts — PR #2860

## Orientation

After a restart, Boss could forget what the operator had just told it: it filed work against a build-and-test host already reported as shut down, then briefed an agent using a setting the operator had already reversed. The coordinator is the long-running Claude Code conversation through which the operator directs work. This change addresses that lost context by giving each replacement conversation a short summary from its predecessor.

The handoff is a rolling note containing facts, decisions, unfinished threads, and prohibitions. The coordinator is instructed to update it during ordinary conversation; the engine, the background service that manages sessions and stores state, saves it and supplies a dated brief as the first prompt of each fresh coordinator. Missing, unreadable, and older notes are identified explicitly. A conversation that survives an engine restart keeps its existing context and receives no handoff. The change spans the Boss session lifecycle, command-line interface, and macOS coordinator instructions.

The revision also addresses four review findings: it shares the existing age formatter, groups session-launch inputs into one context object, reports an unidentified writer as “writer unknown” instead of stale, and corrects documentation about worker access to the commands.

## Change overview

This is a fix for context loss, implemented by adding a durable handoff capability. `boss handoff write <FILE|->` replaces the saved note; `-` reads standard input, allowing a shell heredoc without a scratch file. `boss handoff show` retrieves it later, including after conversation compaction. Both support JSON output through the existing CLI output machinery ([commands and handlers](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/handoff.rs#L16-L117), [output helper](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/output.rs#L184-L199)).

The guarantee supplied by the code is storage, validation, attribution, and delivery of a brief. Refreshing the note, explaining its status to the operator, and confirming facts before acting are instructions to the model, rather than engine-enforced conversational behavior.

## Core implementation overview

The implementation has two connected paths:

- **Write:** coordinator instructions → CLI → engine request handler → one JSON value in the existing database metadata table.
- **Restart:** existing session lifecycle → capture the outgoing session identity → compose a brief from the saved note → pass that brief as the new Claude process’s initial prompt.

A **spawn token** is the identifier assigned to a particular coordinator launch. Matching the note’s recorded writer token against the outgoing session distinguishes that session’s handoff from a note left by an older one. The new `spawned_at` field supplies the previous session’s start time for display; token comparison, not timestamp comparison, determines attribution ([record and timestamp persistence](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/coordinator_tmux.rs#L14-L108)).

## Implementation walkthrough

### 1. Keep the note current before a restart can happen

The new [“Session handoff” prompt section](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift#L439-L463) instructs the coordinator to rewrite the whole note in the same turn as a material fact, decision, prohibition, or open-thread change, and before acknowledging a restart request. It includes a heredoc template. This is a rolling write policy because an abrupt exit cannot reliably run a shutdown hook.

The [auto-mode allowlist](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift#L202-L217) adds the two commands so the routine handoff operation can proceed without waiting on the permission classifier.

### 2. Validate, replace, and attribute the saved note

The CLI sends `SetCoordinatorHandoff { body }` through RPC—the request/response connection to the engine. The [write handler](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L72-L130) trims and validates the body, reads the currently recorded coordinator token, stamps the current time, and stores the result. An absent coordinator record produces an empty writer token. Attribution comes from that record, not a separately authenticated writer identity.

[Storage and validation](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L168-L224) replace one value under `coordinator.handoff`. Blank trimmed bodies and bodies exceeding 16 KiB are rejected. Reads preserve three distinct outcomes:

```rust
pub(crate) enum HandoffState {
    Present(CoordinatorHandoff),
    Missing,
    Unreadable(String),
}
```

([Exact declaration](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L81-L86).)

The [read handler](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L28-L69) returns an optional handoff for present/missing, but a `WorkError` for unreadable data. The [response view](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/protocol/src/types/coordinator_handoff.rs#L19-L33) carries the body, writer, timestamps, age, and whether the current recorded session wrote it. That last flag requires a nonempty exact token match.

Both requests are marked `CoordinatorOnly` by the [worker policy](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/worker-policy/src/policy.rs#L427-L446). The relevant unchanged [access gate](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/trust.rs#L268-L292) applies this policy only to worker-classified connections while `worker_rpc_tier` is enabled; ordinary terminals and other non-worker connections are unaffected.

### 3. Deliver through the existing fresh-session path

**tmux** is the terminal-session manager that hosts the coordinator process. A tmux session can survive while its Claude process has exited, so the lifecycle distinguishes a missing session from a dead process inside an existing session.

[App attachment](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/sessions.rs#L219-L232), the [background restart supervisor](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/server.rs#L581-L594), and [confirmed recreation](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/sessions.rs#L618-L634) now pass a `CoordinatorSpawn` context object. It groups the database, existing-session and creation tmux handles, model, directory, and version probe. Retaining separate tmux handles preserves routing for older sessions hosted on a different server ([context definition](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L69-L86)).

Every path that calls `start_new` supplies a reason: first creation, missing tmux session, exited Claude process, confirmed reset, or model change. A healthy matching-token session follows the existing adoption branch and can receive the existing prompt-change reminder; it does not pass through handoff delivery ([attach and supervisor logic](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L98-L148), [reconciliation](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L330-L383)).

The crucial ordering in [`start_new`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L699-L714) captures the previous identity **before** recording the next launch:

```rust
let previous = work_db
    .coordinator_tmux_record()?
    .map(|record| PreviousSession::from(&record));
```

After recording the new spawn intent, the engine composes and writes `.claude/handoff-brief.txt`. The [launch command](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L728-L743) now appends `{initial_prompt}`; [the successful file-writing branch](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L816-L827) constructs it as:

```rust
format!(
    "\"$(cat {})\"",
    boss_ssh_transport::shell_quote(&path.to_string_lossy())
)
```

Thus the brief becomes a positional prompt argument, using the same shell shape already asserted for [worker launches](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/runner/pane_spawn.rs#L1655-L1673). Delivery does not depend on sending keystrokes to a running pane or on the model deciding to open a file.

### 4. Explain freshness and failures explicitly

The pure [brief composer](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L275-L347) translates storage state and outgoing identity into the following messages:

| Condition                                               | Brief                                                                                         |
| ------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| Nonempty writer token matches the outgoing session      | `HANDOFF PRESENT`, with write time and available previous-session start time                  |
| Writer token is empty, or no previous record exists     | `HANDOFF PRESENT (writer unknown)`; confirm possibly stale facts                              |
| Known writer differs from the recorded outgoing session | `HANDOFF STALE`; the latest note comes from an earlier session                                |
| Missing note on first creation with no previous record  | `NO HANDOFF: none is expected`                                                                |
| Missing note on other starts                            | `NO HANDOFF AVAILABLE`, explicitly distinguishing lost context from nothing worth handing off |
| Stored value cannot be read or decoded                  | `HANDOFF UNREADABLE`, including the error                                                     |

The brief supplies absolute UTC times—timestamps in the common zero-offset time zone—and relative ages. It instructs the incoming model to explain the state, confirm time-sensitive facts before acting, and write its own updated handoff. A transcript directory is mentioned only if it exists, as a last resort ([instructions and directory lookup](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L349-L390)). The CLI shares `format_elapsed_ago` with the composer and formats the absolute display time using the existing CLI timezone convention ([CLI rendering](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/handoff.rs#L94-L115)).

If writing the brief file fails, [the fallback](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L829-L849) supplies a quoted inline prompt naming the error and directing the session to `boss handoff show`. Brief preparation records a `coordinator_handoff_brief` audit event; a file failure adds a `brief_unwritable` outcome. Successful note writes separately record `coordinator_handoff_written`.

### Worked example: a host shutdown survives a dead coordinator

The [restart test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2123-L2159) supplies the concrete fact `- greyarea is shut down (operator said so)`, saved under the current coordinator’s spawn token. Its simulated tmux session still exists with the matching token, but reports the Claude process as dead.

1. **Before:** [reconciliation](https://github.com/spinyfin/mono/blob/91567c86a7195ee78f3e340fbcf6c5bccc5ebedc/tools/boss/engine/core/src/coordinator_tmux.rs#L364-L379) killed the dead session and created another. The [old launch command](https://github.com/spinyfin/mono/blob/91567c86a7195ee78f3e340fbcf6c5bccc5ebedc/tools/boss/engine/core/src/coordinator_tmux.rs#L727-L740) supplied no initial handoff prompt, so this replacement path carried no shutdown fact into the new conversation.
2. **Now:** reconciliation selects `PaneDead`; `start_new` preserves the old identity before assigning the replacement token. The composer finds that the note’s writer matches that preserved identity.
3. **Result:** the replacement receives an initial brief containing `HANDOFF PRESENT: written by the session that just ended`, the process-exit reason, and the shutdown fact. The test asserts a changed session token, a new launch, the fact’s inclusion, and the absence of `HANDOFF STALE`. The model is instructed to acknowledge and confirm the fact before using it.

The contrasting [older-note test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L506-L518) uses a note from `token-a` written three days earlier while the ended session is `token-b`, started two hours earlier. The body remains available, but the brief says `HANDOFF STALE` and exposes the gap. “Present” therefore means attributable to the outgoing session, not a promise that every fact remains current.

## Tests and test infrastructure

**Added tests:**

- [Storage/composer unit tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L434-L593) assert trimming, blank rejection, the exact size boundary, missing/present/corrupt reads, token attribution including empty-token rejection, age text, stale and unknown-writer messages, first-creation handling, unreadable errors, transcript-path encoding, and brief-file creation.
- [Lifecycle tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2091-L2244) pin the positional-prompt command suffix and exercise a dead process with a current handoff, a vanished session without one, an older-session handoff, and operator reset with corrupt stored data.
- [Handler tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs#L81-L147) assert explicit missing results, trimmed write/read round trips, attribution changing after a new session starts, wholesale replacement, invalid writes preserving the prior note, and corruption returning an error.

**Modified tests:** the [metadata round-trip test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/coordinator_tmux.rs#L162-L185) now requires a positive start timestamp. The existing worker-denial test adds [both handoff requests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/worker-policy/src/tests.rs#L324-L334). Protocol test cases add the two response variants and their snake-case tags ([cases](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/protocol/src/wire/frontend_event_wire_tests.rs#L1455-L1468)); the existing [serialization and structural round-trip tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/protocol/src/wire/frontend_event_wire_tests.rs#L2146-L2177) consume those cases.

**Infrastructure:** lifecycle tests reuse fake tmux and temporary directories, adding [context, command-extraction, and brief-reading helpers](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L1208-L1259). The new handler fixture builds a temporary engine state and dispatch context; its response helper requires exactly one response ([fixture](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs#L11-L57)). Existing lifecycle callers and fixtures are adapted to the context struct and timestamp field. No tests are removed, and no build configuration files change.

## Evidence

The PR description reports:

- The author ran `bazel test //tools/boss/... -//tools/boss/app-macos/... -//tools/boss/installer/...`; result: 94 tests pass, with 5,014 passed in the engine core suite, including 24 new tests covering validation, handoff states, attribution, start reasons, launch command shape, and the RPC round trip.
- The author ran `bazel test //tools/boss/app-macos/...`; result: 8 pass, with the Swift prompt literal and allowlist changes compiling and passing.
- The author ran `checkleft run`; result: no findings.
- The author reported that a live coordinator restart against a real `claude` binary was not exercised because a worker may not launch a production engine. The author reported that the launch-command test pins the worker-proven shape.

No screenshots are registered for this task or its revisions.
