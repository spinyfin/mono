# Design: Transcript viewer

- **Status:** Implemented (shipped 2026-05, PRs [#945](https://github.com/spinyfin/mono/pull/945), [#968](https://github.com/spinyfin/mono/pull/968), [#998](https://github.com/spinyfin/mono/pull/998), [#1023](https://github.com/spinyfin/mono/pull/1023), [#1024](https://github.com/spinyfin/mono/pull/1024), [#1030](https://github.com/spinyfin/mono/pull/1030))
- **Audience:** engineers working on the Boss engine, `boss`/`bossctl` CLI, and the macOS desktop app.

## Summary

Let a human operator open and read the agent chat transcript of _any_ execution of a task, in a dedicated window. The operator picks a task, sees the full history of executions run against it (project_design, revision_implementation, ci_remediation, …), selects one, and reads its agent transcript rendered high-fidelity.

The transcript is the Claude Code JSONL session log. The engine converts JSONL → markdown (one converter, shared by the app RPC and the CLI), and the app renders that markdown by reusing the existing Boss markdown rendering component — composed as a _lazy list of per-event markdown segments_ rather than one giant string, so large transcripts stay responsive and individual events (thinking, big tool results) can collapse.

This is an advanced/power-user feature. It is the realization of work related to "future work" in four existing design docs: [worker-live-status](worker-live-status.md) (the live transcript tail; `--format=markdown`, "a dedicated transcript viewer window"), [macos-modernization-audit](macos-modernization-audit.md) (window management — "a window for browsing historical executions and their transcripts"), [work-kanban](work-kanban.md) ("a 'View transcripts' action"), and [markdown-renderer-migration](markdown-renderer-migration.md) (the markdown renderer; pagination for very large documents).

## Goals

- **Read any execution's transcript.** Surface _historical_ runs, not just live ones — retries, revisions, remediations all accumulate as executions on a task and must all be reachable.
- **Execution list per task.** Show every execution for the selected task with its kind, status, model, run-id, and start/end timestamps; let the operator pick one.
- **High-fidelity rendering.** Faithfully render every JSONL event type — user/assistant messages, thinking, tool_use, tool_result, system events, hook/pr-link/attachment events — preserving conversation order, timestamps, model, and code blocks.
- **Stay performant on large transcripts.** Transcripts run to hundreds of messages and hundreds of KB+. Opening and scrolling one must not choke the UI.
- **Engine owns conversion + listing; app is a thin client.** The JSONL → markdown conversion and the execution listing live in the engine. The app lists executions and renders returned markdown; it does not parse raw JSONL itself.
- **Reuse, don't fork.** Reuse the existing markdown rendering component rather than standing up a second markdown renderer ([markdown-renderer-migration](markdown-renderer-migration.md)).
- **Degrade gracefully.** Handle in-progress (partial), missing/rotated/GC'd, and zero-execution cases without erroring.

## Non-goals

- **Editing, replaying, or re-running transcripts.** Read-only. No "resume from here", no annotation/commenting (that is design-renderer future work, not this).
- **A new live-tail experience.** The live agent tail already exists ([worker-live-status](worker-live-status.md)); this viewer is execution-centric and history-first. It renders an in-progress execution's partial transcript (with a low-frequency auto-poll), but it does not replace or restyle the existing 1 Hz tail view.
- **Cross-task / global transcript search or a global transcript browser.** Scope is "the executions of one task." A global browser is possible future work.
- **Changing the JSONL transcript format or how agents emit it.** We consume what Claude Code writes.
- **Exporting transcripts** (PDF/HTML/share). The markdown is already a portable artifact; export is out of scope for v1.
- **Mobile/web frontends.** macOS app only (mirrors the rest of the desktop feature set).

## Background

Context at design time (some of it corrected below to match what implementation actually found):

- **JSONL transcripts.** Claude Code writes one JSON object per line to `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`, where `<cwd-slug>` is the workspace path with `/` and `.` replaced by `-`.
- **Parser.** At design time a JSONL parser lived inside the engine (`agents/transcript.rs`), normalizing lines into `Vec<TranscriptEvent>` via serde-tagged raw enums, tolerating IO errors and skipping malformed/partial lines (the live last line is often a partial write). **As built, that parser moved into the new `boss-transcript-markdown` crate** (see below), which is now the single home for the event model, the parser, and both renderers. `pr-link`, hook events, `attachments`, `stop_hook_summary`, and `turn_duration` are not distinct parser variants — they arrive as `System { subtype, body }` and the converter renders them legibly from the subtype (this proved sufficient; no explicit variants were needed).
- **RPC transport.** JSON-RPC over a unix domain socket. `agents.transcript { run_id }` resolves `run_id` through the **live** supervisor, so it returns "unknown run" for finished (or not-yet-registered) runs — the documented agents-list/transcript-tail divergence. `agents.list` is live-only and has no historical rows. This is why the viewer is keyed on executions, not runs.
- **Durable execution records.** The `work_executions` table is the source of truth for historical runs (`id, task_id, kind, status, model, run_id, started_at, ended_at, …`), with `list_executions` in the work DB returning a task's executions newest-first. The transcript path is snapshotted at spawn on the **`work_runs`** row (`work_runs.transcript_path`, keyed by `execution_id`) — the design originally placed it on `work_executions`; implementation found it on the per-run row and resolves it via `transcript_path_for_execution` (latest run wins).
- **CLI.** `bossctl agents transcript <run-id> --format <text|jsonl>` fetched the tail and formatted locally; `--format` was explicitly reserved for a `markdown` variant, which this project added.
- **Markdown viewer (app).** The app's markdown component is the `StructuredText`/`Textual` stack themed via `.bossMarkdown()` (used by `DesignRendererView`/`MarkdownViewerView`). The design was written against MarkdownUI (`Markdown(md).markdownTheme(.boss)`), but the app has **no MarkdownUI dependency** — the reuse constraint is satisfied against the renderer that actually exists. `MarkdownDocView` renders whole documents eagerly in a plain `ScrollView` — no pagination/laziness — which is exactly why the viewer does not feed it a whole transcript.
- **A bespoke transcript renderer already exists.** `TranscriptTailView` is a `List` of typed rows with `DisclosureGroup`s collapsing thinking/tool sections, polling `agents.transcript` ~1 Hz. Lazy and high-fidelity, but not markdown — the viewer borrows its collapsing/laziness ideas without forking a second renderer.

## Alternatives considered

### Alternative A — Render one big markdown string in the existing `MarkdownDocView`

Convert the whole transcript to a single markdown document and feed it to the existing `MarkdownDocView`. Smallest possible app change.

**Why not (as the whole answer):** `MarkdownDocView` hands the _entire_ string to one renderer view, building the full AST eagerly. For a 300 KB / many-hundred-message transcript that means a multi-second hitch on open and sluggish scrolling — exactly the performance cliff [markdown-renderer-migration](markdown-renderer-migration.md) flags as unsolved. It also can't collapse verbose thinking blocks or truncate huge tool results. So this loses on two explicit goals (performance, collapsible thinking/large output). It did, however, inform the chosen approach.

### Alternative B — App parses JSONL and reuses the bespoke tail renderer

Point the existing lazy, collapsible `TranscriptTailView` machinery at a historical execution.

**Why not:** (1) It pushes rendering fidelity decisions toward the app, fighting the "engine owns conversion; app is a thin client" constraint. (2) It is a _second_ renderer to maintain alongside the markdown viewer — morally the same maintenance burden as forking the markdown renderer. We did borrow its best ideas — lazy-container rendering and `DisclosureGroup` collapsing — in the chosen approach.

### Alternative C — Server-side pagination: engine returns event ranges, app requests pages

Make the transcript RPC page-shaped and have the app fetch pages as the operator scrolls.

**Why not (for v1):** Real complexity — scroll-position bookkeeping, prefetch, jump-to-event across page boundaries, a stateful RPC — for a payload that is fundamentally bounded (a finished transcript is a few hundred KB of text, trivial to transfer once). Lazy _rendering_ on the client solves the actual cost (AST construction); transfer cost is a non-issue. Paging stays in our back pocket for pathological multi-MB transcripts; the shipped implementation has not needed it.

## As-built architecture

**Engine converts JSONL → a structured list of markdown _segments_; the app renders them lazily in a `ScrollView { LazyVStack }`, reusing the `StructuredText` markdown component with per-segment collapsing.** This is Alternative A's engine-owned-conversion + markdown-reuse, fixed with Alternative B's laziness + collapsing, without forking a renderer or pushing parsing into the app.

### Data flow

```
work_runs row (transcript_path, keyed by execution_id)
        │  engine: read jsonl → parse_transcript → events_to_segments
        ▼
execution_transcript RPC ──► [TranscriptSegment{ seq, role, label, timestamp, model,
        │                                        markdown, collapsible, default_collapsed,
        │                                        truncated }]  (+ is_live, complete)
        ▼
app: TranscriptView → ScrollView { LazyVStack } → per segment:
        StructuredText(seg.markdown).bossMarkdown()   ← same renderer, one segment at a time
        wrapped in DisclosureGroup when collapsible
```

### Engine: one converter crate, two callers

The crate **`boss-transcript-markdown`** lives at **`tools/boss/engine/transcript-markdown/`** (engine sub-crates drop the `boss-` prefix from their directory names), with its own `rust_library` + `rust_test` targets and Bazel visibility restricted to the engine ([PR #945](https://github.com/spinyfin/mono/pull/945)). `boss-engine` depends on it and re-exports it (`pub use boss_transcript_markdown as transcript_markdown;`) so in-engine references stay stable.

One deliberate divergence from the original plan: the crate is **fully self-contained** — it owns the JSONL parser (`parse_transcript`), the normalized event model (`TranscriptEvent` / `TranscriptEventKind`), both renderers (`events_to_segments` + `segments_to_markdown` for markdown, `render_text` for the CLI's plain-text format), and `RenderOpts`. The design originally had the crate consuming events from the engine's pre-existing parser; moving the parser in instead means there is exactly one JSONL parser in the tree and all transcript rendering (text + markdown) shares one home. 33 unit tests in the crate cover every event kind.

The crate's public API as shipped:

```rust
pub struct TranscriptSegment {
    pub seq: u64,
    pub role: SegmentRole,        // User | Assistant | Thinking | Tool | System
    pub label: String,            // e.g. "User", "Assistant", "💭 Thinking", "⚙ Bash", "↳ result", "🔗 PR"
    pub timestamp: Option<String>,
    pub model: Option<String>,
    pub markdown: String,         // the rendered body for this one event
    pub collapsible: bool,
    pub default_collapsed: bool,  // thinking + large tool_results start collapsed
    pub truncated: Option<TruncationInfo>,  // { shown_bytes, total_bytes }
}

pub fn parse_transcript(jsonl: &str) -> Vec<TranscriptEvent>;   // lenient; skips malformed/partial lines
pub fn events_to_segments(events: &[TranscriptEvent], opts: &RenderOpts) -> Vec<TranscriptSegment>;
pub fn segments_to_markdown(segs: &[TranscriptSegment]) -> String;   // flatten for the CLI
pub fn render_text(events: &[TranscriptEvent], opts: &RenderOpts) -> String;  // CLI --format=text
```

Two callers, one converter (the "one converter, two callers" constraint holds):

1. **`execution_transcript` RPC (app)** — the engine parses and converts server-side, returning the structured `Vec<TranscriptSegment>` plus `is_live` / `complete`.
2. **`bossctl agents transcript --format=markdown` (CLI)** — the CLI fetches the raw JSONL tail via the existing `agents.transcript` RPC and converts **client-side** through the re-exported crate (`parse_transcript → events_to_segments → segments_to_markdown`). The design originally routed this through a `format` field on the RPC; converting in `bossctl` was simpler and keeps the wire format unchanged. `--format=text` also now routes through the crate's `render_text`, so fidelity never diverges between surfaces.

#### JSONL → markdown mapping (fidelity table)

| Event                                                        | Rendered segment                                                                                                                                                                                    |
| ------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| user text                                                    | role `User`; body = text. Timestamp in the row header.                                                                                                                                              |
| assistant text                                               | role `Assistant`; body = text; model annotation in header.                                                                                                                                          |
| thinking                                                     | role `Thinking`; body = text in a blockquote; `collapsible=true, default_collapsed=true` (verbose, de-emphasized).                                                                                  |
| tool_use                                                     | role `Tool`; label = tool name; input as a fenced code block — `Bash` → ``sh of the command; `Edit`/`Write` → file path + fenced contents/diff; everything else → pretty-printed ``json of `input`. |
| tool_result                                                  | role `Tool` (`↳ result`); output in a fenced code block; `is_error` flagged; large output truncated to `RenderOpts.max_result_bytes` with `truncated` set and `collapsible=true`.                   |
| system (`stop_hook_summary`, `turn_duration`, hook payloads) | role `System`; subtype as label; body as a de-emphasized blockquote. The `init` line is skipped.                                                                                                    |
| `pr-link` (a `System` subtype)                               | role `System`, label `🔗 PR`; body = a markdown link to the PR.                                                                                                                                     |
| unknown / malformed                                          | skipped by `parse_transcript`; never aborts.                                                                                                                                                        |

Conversation order = `seq`. Code blocks are preserved verbatim (the converter never re-wraps inside fences). Timestamp/model are passthrough. Rendering system subtypes from `System { subtype, body }` proved sufficient — no explicit parser variants were added.

#### Execution resolution — read from durable rows, not the live supervisor

The RPCs resolve transcripts from durable DB rows, sidestepping the "unknown run" divergence entirely ([PR #968](https://github.com/spinyfin/mono/pull/968)):

1. **Execution listing** needed no new RPC at all: the pre-existing `ListExecutions { work_item_id }` request / `ExecutionsList` reply already served it (backed by `list_executions`, newest-first). The design proposed a new `executions.list`; reusing the existing endpoint resolved the "duplicate of the detail payload?" open question by adding nothing. One wire-contract lesson: the app initially sent `task_id` instead of `work_item_id`, which the engine silently ignored (filter unset → every task's executions returned, reply dropped) — fixed with a regression test in [PR #1023](https://github.com/spinyfin/mono/pull/1023).
2. **`ExecutionTranscript { execution_id }`** (new request; handler in `engine/core/src/app/executions.rs`):
   - looks up the execution, computes `is_live` from `finished_at`/status;
   - resolves the path via `transcript_path_for_execution` (latest `work_runs` row for the execution). If no path was ever recorded, or the file is absent (rotated/GC'd/never started), it replies with the typed `ExecutionTranscriptUnavailable { execution_id, reason }` event — a graceful state, not a `WorkError`. The design's fallback of recomputing `~/.claude/projects/<cwd-slug>/<session_id>.jsonl` from `session_id` was dropped: the path is snapshotted at spawn on every run row, so a missing path means there is nothing trustworthy to recompute from, and Unavailable is the honest answer;
   - otherwise reads the file, runs `parse_transcript → events_to_segments`, and replies `ExecutionTranscriptResult { execution_id, segments, is_live, complete }`. The proposed `format?` request field was dropped — the app is the only structured consumer, and the CLI converts client-side.

Keying on `execution_id` (a stable, non-null PK) rather than `run_id` (nullable, supervisor-coupled) is what makes historical, retry, and remediation executions all reachable.

**Protocol additions** (`boss-protocol`): `SegmentRole`, `TruncationInfo`, `TranscriptSegment` wire types; `FrontendRequest::ExecutionTranscript`; `FrontendEvent::ExecutionTranscriptResult` / `ExecutionTranscriptUnavailable`.

### App: the transcript viewer window

A value-keyed scene in `BossMacApp.swift` ([PR #998](https://github.com/spinyfin/mono/pull/998)):

```swift
struct TranscriptViewerRef: Codable, Hashable { var taskId: String; var preselectExecutionId: String? }
// Hashable/Equatable are custom, over taskId ONLY — see "window identity" below.

WindowGroup(id: "transcript-viewer", for: TranscriptViewerRef.self) { $ref in
    if let ref { TranscriptViewerView(ref: ref) }   // defaultSize 900×640
}
```

**Window identity:** the design flagged that keying the window on the full ref would spawn a second window when the same task is opened with a different preselection. As built, `TranscriptViewerRef` hashes/compares on `taskId` only, so re-invoking "View transcripts" for a task focuses the existing window; `preselectExecutionId` rides along without affecting identity.

`TranscriptViewerView` is a `NavigationSplitView`:

- **Left — execution list.** `List(executions, selection:)` of `ExecutionRow`s over `ExecutionVM` (id, kind, status, model, runId, startedAt, endedAt), fed by `ListExecutions`. Loading spinner while the RPC is in flight; `ContentUnavailableView` when the task has zero executions. Selection (including `preselectExecutionId`) loads that execution's transcript.
- **Right — transcript pane.** `TranscriptView` ([PR #1023](https://github.com/spinyfin/mono/pull/1023)) renders the segments:

  ```swift
  ScrollView {
      LazyVStack(alignment: .leading, spacing: 0) {
          ForEach(segments) { seg in
              if seg.collapsible {
                  DisclosureGroup(isExpanded: binding(seg)) {
                      StructuredText(seg.markdown).bossMarkdown()
                  } label: { SegmentHeader(seg) }   // role-coloured label, model, local timestamp
              } else {
                  SegmentHeader(seg)
                  StructuredText(seg.markdown).bossMarkdown()
              }
          }
      }
  }
  ```

  This reuses the one `StructuredText`/`Textual` markdown renderer + Boss theme (no second markdown renderer — constraint satisfied), but renders **one segment at a time inside a lazy container**, so ASTs are only built for viewport-near rows, and verbose thinking/large results collapse per the engine's `default_collapsed`. Truncated tool results get a "Showing N of M" footer driven by the segment's `truncated` byte counts. A jump-to-turn menu (user turns, via `ScrollViewReader`) provides navigation. `MarkdownDocView` is untouched and continues to serve design docs.

  **Why `LazyVStack` and not `List`:** the design's Risk #1 required spiking lazy behaviour before committing, with "manual windowing" as the documented fallback. The spike (a synthetic 500-segment transcript with a render probe, kept as a regression test) measured `List` building **500 of 500** segment ASTs on open — variable-height markdown rows force `List` to measure every row — versus **17 of 500** for `ScrollView { LazyVStack }`. The fallback shipped; the transcript pane has no per-row selection, so nothing was lost, and the test's laziness bound catches any regression back to an eager container.

- **`EngineClient` / `ChatViewModel`** gained `sendListExecutions(taskId:)` (sends `work_item_id`) and `sendExecutionTranscript(executionId:)`, the corresponding `executionsList` / `executionTranscriptResult` / `executionTranscriptUnavailable` events, and `executionsByTaskID` / `transcriptsByExecutionID` caches with `loadExecutions`, idempotent `loadTranscript`, and `refreshTranscript`.

### Invocation surfaces

Both surfaces shipped ([PR #1024](https://github.com/spinyfin/mono/pull/1024)), landing on the components that actually exist in the app (the design's `TaskCardView`/`TaskDetailView` names predated the work-board naming):

- **Task card context menu** (`WorkBoardCardItem`) — `Button("View transcripts…")` alongside "Copy ID"; opens the window with no preselection. The fast power-user path.
- **Task detail popover** (`WorkCardPopoverView`) — a new `executionsSection` after Dependencies. This was added scope: the design assumed the detail view already listed executions, but the popover had no such section, so task 5 built it — header with a "View transcripts…" link button, spinner while loading, "No executions yet." empty state, and clickable `ExecutionRow`s that open the window with that execution **preselected**.

Two surfaces, one window; window de-duplication (taskId-only identity) makes repeat invocations focus rather than duplicate.

### Live / partial / missing / empty handling

All graceful states shipped across tasks 3–6 ([PR #1030](https://github.com/spinyfin/mono/pull/1030) closed the loop):

- **Live (in-progress):** `is_live=true`; the partial transcript renders (`parse_transcript` drops the half-written last line) under a "Still running — partial transcript" banner with a manual Refresh, plus a **5-second auto-poll**: `.task(id: doc.isLive)` re-fetches while live and is cancelled by SwiftUI automatically the moment `is_live` flips false. The design suggested "reusing the tail's cadence (~1 Hz)"; 5 s was chosen deliberately — a full-transcript re-fetch re-parses and re-renders the whole segment list, so 1 Hz is too aggressive, and 5 s matches the Metrics pane's cadence. No polling ever runs against a completed transcript.
- **Missing/rotated/GC'd/never-recorded:** `ExecutionTranscriptUnavailable { reason }` → a `ContentUnavailableView` showing the engine's reason, not a crash.
- **Zero executions:** empty-state `ContentUnavailableView` in the list pane; placeholder in the transcript pane when nothing is selected.
- **Huge single tool_result:** engine truncates to `RenderOpts.max_result_bytes` with `truncated` metadata; the collapsed segment shows the "Showing N of M" affordance. Full-output retrieval remains a follow-up if operators ask for it.

## Design questions — how they resolved

- **MarkdownUI laziness inside `List`/`LazyVStack` (Risk #1):** spiked as required; `List` over-materialized (500/500) and the documented fallback (`ScrollView { LazyVStack }`, 17/500) shipped, guarded by a regression test. See "Why `LazyVStack` and not `List`" above.
- **Segment granularity:** per-event shipped for v1, as recommended. Per-turn grouping remains a revisit-after-dogfooding option.
- **System-event fidelity:** rendered from `System { subtype, body }` — legible labels (`🔗 PR` links, blockquoted hook/duration notes) without new parser variants. Labeling has not proved lossy.
- **`executions.list` vs. embedding in task detail:** dissolved — the pre-existing `ListExecutions` RPC already served both; nothing was duplicated.
- **CLI scope:** option (a) shipped — `markdown` joined the existing run-id-keyed `bossctl agents transcript --format` enum. Option (b), a durable execution-keyed CLI command (`bossctl executions transcript <execution-id>`), was explicitly deferred as a follow-up and has not been built; the CLI therefore still cannot read finished runs' transcripts (the app can).
- **Window identity:** resolved via taskId-only `Hashable`/`Equatable` on `TranscriptViewerRef` — same-task invocations focus the existing window regardless of preselection.
- **Auth/PII:** unchanged. Transcripts can contain secrets the agent saw; the window surfaces them to anyone at the operator's machine. Accepted for a local power-user tool; no redaction.

## Implementation history

Shipped as six PRs matching the planned breakdown (order 1 → 2 → 3 → 4 → 5 → 6):

1. [#945](https://github.com/spinyfin/mono/pull/945) — `boss-transcript-markdown` crate (parser + segment/text renderers, 33 unit tests, engine re-export).
2. [#968](https://github.com/spinyfin/mono/pull/968) — `ExecutionTranscript` RPC + protocol types; execution listing via existing `ListExecutions`; CLI `--format=markdown`.
3. [#998](https://github.com/spinyfin/mono/pull/998) — transcript viewer window, `TranscriptViewerRef` (taskId-only identity), execution list pane, `EngineClient`/`ChatViewModel` wiring.
4. [#1023](https://github.com/spinyfin/mono/pull/1023) — lazy segmented `TranscriptView` (`LazyVStack` per the laziness spike), collapsing, truncation affordance, jump-to-turn; fixed the `work_item_id` wire mismatch from task 3.
5. [#1024](https://github.com/spinyfin/mono/pull/1024) — invocation surfaces: `WorkBoardCardItem` context menu + `WorkCardPopoverView` executions section with preselecting rows.
6. [#1030](https://github.com/spinyfin/mono/pull/1030) — 5 s auto-poll for live transcripts (graceful states had already landed in 3–5).

(end)
