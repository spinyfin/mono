**Orientation**

PR #2860, “boss: coordinator session handoff across restarts,” preserves a short summary of operator instructions when Boss replaces its coordinator session. This guide compares merge base `91567c86` with head `725bd759`, using the implementation and tests in the checkout.

**1. The problem being solved**

Boss already knew how to replace a missing or dead coordinator process. What it did not preserve was the conversation: facts the operator had stated, decisions, unfinished threads, and explicit prohibitions.

That creates a practical failure mode: a replacement coordinator can investigate a host the operator deliberately shut down, or brief another agent using settings the operator already changed. The PR description identifies those as the motivating incidents.

The important distinction is between **replacing** and **adopting** a session:

- A fresh Claude process has lost the previous conversation and needs a handoff.
- A surviving Claude process retains its conversation. Adopting it after an engine restart still uses the existing prompt-change nudge when necessary.

**2. The core implementation**

The change creates a complete write–store–deliver path:

```text
Coordinator maintains a rolling brief
    → boss handoff write <file|->
    → engine metadata: body + timestamp + writer session token
    → fresh coordinator startup composes a dated status report
    → report becomes Claude’s positional initial prompt
```

The coordinator writes the summary; the engine stores and delivers it. The engine does not reconstruct knowledge from transcripts.

The decisive change is in [`start_new`](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_tmux.rs:680). Before overwriting the coordinator singleton with the replacement’s identity, it captures the outgoing session:

```rust
let previous = work_db
    .coordinator_tmux_record()?
    .map(|record| PreviousSession::from(&record));
```

After recording the new spawn intent, it prepares the initial prompt using that captured identity:

```rust
let initial_prompt =
    prepare_session_start_brief(work_db, working_directory, previous.as_ref(), reason);
```

The launch command previously ended at `--permission-mode auto`. It now appends `{initial_prompt}`. Normally, that argument expands to the contents of `.claude/handoff-brief.txt`:

```sh
exec claude --model <model> --permission-mode auto \
  "$(cat '<session directory>/.claude/handoff-brief.txt')"
```

This ordering matters: comparing the writer against the **new** session token would incorrectly classify the outgoing session’s own handoff as an older session’s note.

The engine guarantees preparation and inclusion of the brief in the launch command. Keeping its contents complete, acknowledging it, and verifying facts remain instructions to the model.

**3. Walkthrough of the major changes**

**Writing and storing the handoff.** The new [CLI module](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/cli/src/handoff.rs:13) provides `boss handoff write <FILE|->` and `boss handoff show`, using the existing RPC and JSON-output helpers. Each write replaces the entire previous handoff.

The [RPC handlers](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/app/coordinator_handoff.rs:73) trim the body, reject blank content and content exceeding **16 KiB after trimming**, then stamp it with the engine’s current time and recorded coordinator token. The caller supplies only the text.

The stored record is deliberately small:

```rust
pub(crate) struct CoordinatorHandoff {
    pub(crate) body: String,
    pub(crate) written_at: i64,
    pub(crate) writer_spawn_token: String,
}
```

It occupies one existing metadata-table entry, `coordinator.handoff`. Separately, [coordinator spawn metadata](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/work/coordinator_tmux.rs:84) gains `coordinator.tmux_spawned_at`, written in the same transaction as the spawn intent. Older records can lack that timestamp.

**Explaining what survived.** The new [`coordinator_handoff` module](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_handoff.rs:266) separates storage results into `Present`, `Missing`, and `Unreadable`. Its pure brief composer then distinguishes these user-facing cases:

| Situation                                                 | What the incoming session receives                                       |
| --------------------------------------------------------- | ------------------------------------------------------------------------ |
| Writer matches the outgoing session                       | `HANDOFF PRESENT`, with write time and outgoing-session start time       |
| Known writer differs from the outgoing session            | `HANDOFF STALE`, explaining that the outgoing session never refreshed it |
| Writer is empty, or no previous session can be identified | `HANDOFF PRESENT (writer unknown)`, with instructions to confirm facts   |
| Nothing stored on first creation                          | `NO HANDOFF: none is expected`                                           |
| Nothing stored on a replacement                           | `NO HANDOFF AVAILABLE`, explicitly warning that context was lost         |
| Storage read or JSON decoding fails                       | `HANDOFF UNREADABLE`, including the error                                |

Staleness is based on **session identity**, not an age threshold:

```rust
let by_previous =
    previous.is_some_and(|p| !p.spawn_token.is_empty()
        && p.spawn_token == handoff.writer_spawn_token);
```

Even a matching handoff is described as accurate only “as of” its write time. Every brief instructs the coordinator to summarize the state, verify infrastructure facts before acting, and write a fresh handoff.

`boss handoff show` compares the writer with the **currently recorded** session instead. Consequently, a successor can receive a valid outgoing-session handoff at startup while `show` correctly reports that the successor itself has not yet rewritten it.

**Worked example: Claude exits but tmux survives.** This follows the [dead-pane regression test](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_tmux.rs:2124). `A` and `B` below are explanatory labels for generated tokens.

Input and relevant state:

- The recorded coordinator has token `A`.
- Its stored handoff says `- greyarea is shut down (operator said so)` and has writer token `A`.
- The tmux session still exists and reports token `A`, but `PaneDead` is `"1"`.

Before this PR, the equivalent crash path verified the token, killed the dead tmux session, and launched a replacement without a conversational handoff argument. The operator’s earlier shutdown statement was not carried forward by this startup path.

With this PR:

1. `reconcile_existing` verifies token `A`, removes the dead session, and calls `start_new` with `PaneDead`.
2. `start_new` captures outgoing identity `A`, then records replacement identity `B`.
3. The composer compares stored writer `A` with captured outgoing identity `A`.
4. The generated brief contains both:
   ```text
   HANDOFF PRESENT: written by the session that just ended
   ```
   and:
   ```text
   - greyarea is shut down (operator said so)
   ```
5. The replacement’s launch command supplies that brief as its initial prompt.

The observable result asserted by the test is a new token, a new-session command, and a brief containing the shutdown fact and process-exit explanation, without `HANDOFF STALE`. The test does not assert that a real model subsequently avoids filing a chore.

A useful boundary is covered by the adjacent stale-handoff test: if the stored writer is `"an-older-session"` rather than `A`, startup preserves the old text but labels it `HANDOFF STALE`. Thus “a note exists” does not silently become “the outgoing conversation was preserved.”

**Lifecycle integration and failure handling.** A new [`CoordinatorSpawn`](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_tmux.rs:78) context groups the database, existing-session tmux handle, creation tmux handle, model, directory, and version probe. Attach, supervisor restart, and explicit recreation callers now pass this context. Keeping the two tmux handles separate preserves existing legacy-server routing.

Every fresh-start path supplies a reason: first creation, missing tmux session, exited Claude process, operator reset, or model change. Healthy-session adoption continues through the existing nudge path.

[`prepare_session_start_brief`](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_tmux.rs:781) also handles a failed brief-file write: it supplies a shell-quoted inline notice directing the new session to `boss handoff show`. Successful handoff writes and brief preparation are audited. Existing transcript directories are mentioned only as a last-resort recovery source.

**Making the write happen.** The [Swift coordinator prompt](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift:440) instructs the model to refresh the handoff in the same turn as relevant facts, decisions, prohibitions, or thread changes, and before acknowledging restart requests. This avoids depending on a shutdown hook after an abrupt termination.

The app also pre-clears the handoff CLI commands in auto mode. At the engine boundary, both new request verbs are denied to workers as `CoordinatorOnly`; response sanitization adds the corresponding event variants.

**4. Tests and supporting infrastructure**

The added coverage falls into three main groups:

- **Storage and composition:** [13 unit tests](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_handoff.rs:435) cover trimming, blank and size limits, missing/present/corrupt storage, token attribution, age rendering, present/stale/missing/unreadable briefs, unknown attribution without a previous record, transcript-path encoding, and writing the brief file.
- **Coordinator lifecycle:** [five new async tests](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/coordinator_tmux.rs:2092) cover the exact initial-prompt command shape, dead-pane recovery, missing-session recovery without a handoff, stale attribution, and an operator reset with corrupt stored data.
- **RPC handlers:** [five new async tests](/private/tmp/boss-review-guide-corpus-793/checkouts/mono-2860/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs:82) cover empty reads, write/read attribution across session changes, whole-record replacement, rejected writes preserving the previous record, and corruption returning `WorkError`.

Existing tests also gain spawn-timestamp assertions, worker-denial cases, and serialization cases pinning `coordinator_handoff_result` and `coordinator_handoff_set`.

The test infrastructure remains hermetic: existing fake tmux and version probes are reused. New helpers construct `CoordinatorSpawn`, extract the recorded launch command, and read the generated brief. Handler tests build temporary server/database state and assert exactly one response through a session sink; they are direct handler tests, not a live CLI-to-engine transport test.

I did not run the suites for this guide. The PR reports passing Bazel suites and `checkleft run`, but explicitly reports no live restart with a real Claude binary. The tests verify storage, classification, and launch construction; they do not verify real-model compliance. The brief-file failure fallback also has no direct added regression test in this diff.

https://github.com/spinyfin/mono/pull/2860
