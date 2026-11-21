1. **The problem: a recovered coordinator process lost the conversation’s operational context.**

   Boss already knew how to replace a coordinator whose Claude process exited or whose tmux session disappeared. But the replacement launched without anything the operator had told its predecessor: hosts deliberately shut down, settings changed, decisions made, or actions prohibited.

   The motivating example is a coordinator filing work against a host the operator had already reported as shut down. Restart recovery restored the agent’s ability to work, but not the knowledge needed to choose the right work.

   This applies to **fresh Claude sessions**. If the engine restarts while the coordinator survives, Boss adopts that existing session, which retains its conversation.

2. **The implementation: persist a rolling handoff and make it the replacement’s first prompt.**

   The change connects three pieces:

   - The coordinator writes a short summary through `boss handoff write <FILE|->`.
   - The engine stores it in its existing SQLite metadata table.
   - Every fresh coordinator launch receives an engine-composed brief containing that summary, its provenance, and instructions for handling missing or outdated information.

   The stored record is deliberately small ([storage implementation](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L64)):

   ```rust
   pub(crate) struct CoordinatorHandoff {
       pub(crate) body: String,
       pub(crate) written_at: i64,
       pub(crate) writer_spawn_token: String,
   }
   ```

   It lives under `coordinator.handoff`; each write replaces the entire previous value. The engine trims the body, rejects blank input, and enforces a **16 KiB byte limit after trimming**. Timestamp and writer token come from the engine.

   The [new coordinator prompt instructions](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift#L440) require a rewrite in the same turn as relevant facts, decisions, prohibitions, or open-thread changes, and before acknowledging a restart request. This is a rolling write because a crash cannot reliably run a shutdown hook. The app also pre-clears the handoff commands in auto mode.

   **The important limit:** maintaining an accurate summary is a model instruction. The engine enforces storage, validation, attribution, and startup delivery; it does not extract facts from the conversation or verify that the coordinator recorded everything.

3. **The main logic changes, with a worked restart example.**

   Start review at [`start_new`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L680). Its ordering is central: it captures the outgoing session’s identity **before** recording the replacement’s spawn intent.

   ```rust
   let previous = work_db
       .coordinator_tmux_record()?
       .map(|record| PreviousSession::from(&record));
   ```

   After recording the new intent, it prepares the initial prompt using that captured identity:

   ```rust
   let initial_prompt =
       prepare_session_start_brief(work_db, working_directory, previous.as_ref(), reason);
   ```

   Otherwise, comparing the stored writer against the newly recorded session would misclassify a valid handoff from the outgoing session.

   [`prepare_session_start_brief`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L782) reads the handoff, composes the brief, and writes `.claude/handoff-brief.txt` in the coordinator session directory. The decisive launch change is:

   ```sh
   # Before: no positional initial prompt
   exec claude --model <model> --permission-mode auto

   # After: the brief is the positional initial prompt
   exec claude --model <model> --permission-mode auto \
     "$(cat '<session dir>/.claude/handoff-brief.txt')"
   ```

   These excerpts omit the unchanged environment setup. The new form follows the existing worker launch mechanism and supplies the brief without timing a later tmux keystroke injection.

   **Worked example — based on the tests.** The [brief-composer test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L488) uses this relevant state:

   | Input/state              | Value                                                   |
   | ------------------------ | ------------------------------------------------------- |
   | Outgoing session token   | `token-a`                                               |
   | Outgoing session started | One hour ago                                            |
   | Handoff writer           | `token-a`                                               |
   | Handoff written          | Four minutes ago                                        |
   | Body                     | `greyarea is shut down …` and `tmux hosting re-enabled` |
   | Restart reason           | `PaneDead`                                              |

   The [lifecycle test](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2124) exercises the corresponding situation: tmux still exists with the matching token, but reports its pane dead.

   **Before:** reconciliation verified the token, killed the dead session, and launched a fresh Claude process with the old command above. That replacement received no handoff containing the shutdown fact.

   **After:** reconciliation takes the same recovery path but passes `CoordinatorStartReason::PaneDead`. Startup captures `token-a`, records a new session token, and compares the captured token against the handoff’s writer:

   ```rust
   let by_previous =
       previous.is_some_and(|p| !p.spawn_token.is_empty()
           && p.spawn_token == handoff.writer_spawn_token);
   ```

   Because they match, the brief reports `HANDOFF PRESENT: written by the session that just ended`, includes the four-minute age and absolute timestamp, explains that the Claude process exited, and includes the shutdown fact. The replacement receives this text as its first prompt. It is instructed to summarize it, confirm time-sensitive facts before acting, and write its own refreshed handoff.

   The observable change verified by tests is the brief’s contents and the launch argument carrying it. A real model’s subsequent decisions are outside those assertions.

   **The useful boundary:** if the outgoing session is `token-b` but the stored writer remains `token-a`, the composer reports `HANDOFF STALE` and retains the old body with a warning to ask what changed. Staleness here is determined by **session identity, not an age threshold**. Even a matching handoff is described as accurate only “as of” its write time.

   The remaining architecture supports that contract:

   - **Explicit states and failure handling.** [`HandoffState`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L76) distinguishes present, missing, and unreadable storage. The composer further distinguishes matching, stale, and unknown writers. Missing on first creation is expected; missing after replacement warns about lost context. If writing the brief file fails, startup supplies an inline error notice directing the session to `boss handoff show`.
   - **Lifecycle context.** `CoordinatorStartReason` distinguishes first creation, missing tmux session, dead Claude process, operator reset, and model change. `CoordinatorSpawn` groups existing lifecycle dependencies, accounting for much of the mechanical diff. Live-session adoption still uses the existing prompt-change nudge and receives no startup brief.
   - **Durable timing.** [`CoordinatorTmuxRecord`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/work/coordinator_tmux.rs#L17) gains `spawned_at`, written with the spawn-intent metadata transaction. Older records retain an unknown start time.
   - **CLI and RPC.** [`handoff.rs`](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/cli/src/handoff.rs#L16) adds write/show commands using existing RPC and JSON-output helpers. The [handlers](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/coordinator_handoff.rs#L28) return an explicit empty result for missing data and an error for unreadable data. Both requests are coordinator-only at the worker-policy gate.
   - **Diagnostics.** Writes and startup briefs generate audit events. An existing transcript directory may be mentioned as a last resort; transcripts are not automatically replayed.

4. **Tests added and test infrastructure changed.**

   | Area                                                                                                                                                                | What the change verifies                                                                                                                                                                                    |
   | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
   | [Handoff unit tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_handoff.rs#L403)         | Trimming, blank rejection, exact size boundary, storage states, token attribution, present/stale/missing/unreadable brief text, first-creation behavior, transcript-path encoding, and brief-file creation. |
   | [Coordinator lifecycle tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/coordinator_tmux.rs#L2089)  | Exact initial-prompt command shape; dead-pane restart carrying the outgoing handoff; missing-session restart without a handoff; stale attribution; operator reset with corrupt storage.                     |
   | [RPC-handler tests](https://github.com/spinyfin/mono/blob/725bd759480d416ffffcd39c32ab5fa99d3acc97/tools/boss/engine/core/src/app/tests/coordinator_handoff.rs#L81) | Write/read round trip, attribution changing across sessions, wholesale replacement, invalid writes preserving prior data, and corrupt storage producing an error.                                           |
   | Existing suites extended                                                                                                                                            | Spawn metadata now asserts a start timestamp; worker-policy tests deny both new requests; protocol tests pin both response-event tags.                                                                      |

   Infrastructure changes are modest. Lifecycle tests reuse the existing fake tmux runner, adding `spawn_ctx`, a helper to extract the recorded launch command, and a helper to read the generated brief. Handler tests build temporary server/database state and use `SessionSink` to assert exactly one response. Pure composer tests inject a fixed time, keeping age assertions deterministic.

   I inspected the code and tests without running them. The PR reports successful Bazel and checkleft runs, but explicitly no live restart against a real Claude binary. The added tests also do not directly exercise the inline fallback for an unwritable brief file.
