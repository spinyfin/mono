# Boss: Comments in the Markdown Viewer

Design doc for the in-app comment system (P529) that lets the user
highlight a region of any rendered markdown surface in Boss — work-item
descriptions, the popped-out design-doc viewer — attach a comment, see
it in a sidebar, and have the engine act on it. This revision is the
**as-built** record: the project shipped in four phases plus a UI
refinement pass, and its original "magic wand" dispatch path was
subsequently built, found wanting, and retired in favour of
[`comment-triggered-document-revisions`](comment-triggered-document-revisions.md).

- **Status:** shipped; magic-wand portion retired and superseded.
- **Shipped in:** [#605](https://github.com/spinyfin/mono/pull/605)
  (Phase 1 UI shell), [#622](https://github.com/spinyfin/mono/pull/622)
  (UI refinements), [#915](https://github.com/spinyfin/mono/pull/915)
  (Phase 2 engine persistence + anchoring; renderer wiring followed in
  a separate PR — `CommentWire.swift`),
  [#970](https://github.com/spinyfin/mono/pull/970) (Phase 3 magic
  wand, engine-owned docs), [#1106](https://github.com/spinyfin/mono/pull/1106)
  (Phase 4 magic wand, PR-backed docs).
- **Superseded parts:** the per-comment magic wand (Phases 3–4) was
  removed by the intent-classifier design; see
  [Magic wand — built, then retired](#magic-wand--built-then-retired).

## Goals

- Any markdown surface the macOS app renders can be commented on. The
  surfaces shipped are the expanded work-item description
  (`MarkdownViewerView` in `tools/boss/app-macos/Sources/DesignsView.swift`)
  and the design-doc viewer (`DesignRendererView`); each opts in with a
  one-line `.withComments()` modifier, preserving the "adding a future
  surface is a one-line change" contract.
- Selecting a span of rendered text exposes an authoring affordance;
  the commented region stays visually highlighted in the doc.
- Comments persist in the engine and survive app restarts, doc edits,
  and view re-opens.
- Comments stay anchored to the originally selected text even when the
  underlying markdown is edited. Line-number-only anchoring is
  explicitly rejected as too fragile.
- A right-side sidebar lists comments for the open doc with snippet,
  timestamp, dismiss action, and click-to-jump back to the anchored
  text in the doc.
- Comments drive engine-owned handling of the operator's intent. As
  originally designed this was a per-comment "magic wand" dispatch
  (isolated Claude for engine-owned docs, chore worker for PR-backed
  docs); as-built that path shipped and was then replaced by intent
  classification and the revision/answer-agent flow (successor design).
- The interaction model was validated end-to-end on real docs before
  the engine learned to persist comments or dispatch agents; Phase 1
  shipped UI only.

## Non-goals

- Commenting on PR diff views, chat panes, or the kanban-card preview
  text.
- Free-form threaded replies. v1 shipped single-level comments; the
  successor design later added _structured_ thread entries
  (`comment_thread_entries` — engine nudges, answer-agent replies,
  operator follow-ups), which is narrower than general nesting.
- A general "show me previous versions of this work-item description"
  feature. Only the narrow doc-version CAS needed for safe applies is
  in scope (see [Versioning](#versioning)).
- Inline diff rendering of agent-proposed changes. The Phase 3 preview
  showed two full renders side-by-side; no intra-paragraph diff ever
  shipped (and the preview sheet itself was deleted with the wand).
- Multi-user permissions. Boss is single-user; anyone can dismiss any
  comment. The schema records `author` so a future ACL layer has a
  hook — as-built, the UI never grew an author field, so authorship is
  effectively constant.
- Realtime collaboration (live presence, OT/CRDT).
- Auto-applying agent output. This held throughout: the magic wand
  required explicit Apply, and the successor design goes further —
  it only _nudges_ toward a revision, never applies.

## Alternatives considered

### Alternative A: GitHub-style line-anchored comments

Anchor every comment to a `(file_path, line_number, column_range)`
tuple, the way GitHub PR review comments do.

- **Pros**: trivial to serialize; matches an existing mental model;
  the line-range is what `gh pr comment` uses, so a future export-to-
  GitHub path would be easy.
- **Cons**: any non-trivial edit invalidates every comment below the
  edit point. The motivating use case for this feature is exactly the
  case where the doc keeps changing — the human comments, an agent
  edits, the human re-reads. Line-anchored comments would re-attach to
  the wrong text on every edit. The problem is well-studied: the W3C
  Web Annotation Data Model exists specifically because line-anchoring
  fails on edited docs.
- Rejected.

### Alternative B: Per-doc inline `<!-- comment: ... -->` markers stored in the markdown source

Store comments as HTML-comment-shaped markers embedded in the markdown
itself.

- **Pros**: zero new schema. Comments travel with the doc trivially
  (a PR-backed doc literally carries its comments in git).
- **Cons**: visible to anyone editing the raw markdown — including
  workers, which would pollute their prompts. Comments aren't really
  the doc's content; mixing them in violates the separation between
  artifact and conversation. Dismissed comments leave residue.
  Anchoring is still required. Worst of all: a doc-editing agent would
  see its own comment in the markdown it's asked to edit.
- Rejected.

### Alternative C: W3C Web Annotation `TextQuoteSelector` anchoring in the engine, comments stored as engine rows

What shipped. Comments live in a `work_comments` table keyed to the
artifact (work-item id, or PR-doc ref). Each comment carries a
`TextQuoteSelector`-shaped anchor (`exact`, `prefix`, `suffix`),
re-resolved against the current doc text on load.

- **Pros**: anchoring is robust to most realistic edits because the
  prefix/suffix context disambiguates even when the exact text recurs.
  Comments are first-class engine objects so they show up in the
  subscription stream, survive crashes, and feed naturally into the
  existing attention/inbox plumbing.
- **Cons**: anchoring isn't free — each load runs a quote match, and
  the engine stores ~80–200 bytes of selector per comment. Both costs
  proved bounded in practice.
- **Chosen.** Detailed below.

## Chosen approach

### Phasing (as shipped)

The UI interaction model was validated before the agent path was
built, as planned — though the phases interleaved differently than the
strict 1→2→3→4 ladder the plan assumed:

- **Phase 1 — UI shell** ([#605](https://github.com/spinyfin/mono/pull/605)):
  selection → comment → sidebar → dismiss, in-memory only. Shipped
  _without_ the planned `(line, offset, length)` anchoring — Phase 1
  stored only the verbatim quoted text — and with the doc highlight
  stubbed (see [macOS app](#macos-app-architecture)). Also added the
  first Swift test target run in CI
  (`//tools/boss/app-macos:BossTests`), unplanned scope.
- **UI refinement pass** ([#622](https://github.com/spinyfin/mono/pull/622)):
  a validation-driven pass the original phasing didn't anticipate —
  entry triggers, popover, Return-to-submit, persistent highlight,
  click-to-jump, dismiss placement. This is where several sidebar/
  popover decisions below diverged from the original spec.
- **Phase 2 — persistence + resilient anchoring**
  ([#915](https://github.com/spinyfin/mono/pull/915)): engine schema,
  `comments_*` RPCs, subscription topic, resolver, soft-dismiss,
  cross-doc migration. #915 deliberately shipped only the
  engine/protocol half to stay independently reviewable; the macOS
  wiring (`CommentWire.swift` + `EngineClient` support) followed in a
  separate PR.
- **Phase 3 — magic wand, engine-owned docs**
  ([#970](https://github.com/spinyfin/mono/pull/970)): the specialised
  no-tools Claude call, dispatch table, preview sheet, CAS apply.
  Landed engine-first _before_ the app's Phase 2 wiring existed, so it
  was never operable end-to-end from the app — a dependency inversion
  the phasing didn't anticipate. A 13-line attribution follow-up
  ([#1102](https://github.com/spinyfin/mono/pull/1102)) was still open
  when the wand was retired.
- **Phase 4 — magic wand, PR-backed docs**
  ([#1106](https://github.com/spinyfin/mono/pull/1106)): `pr_doc`
  dispatch arm creating a chore. Shipped without the PR-resume
  integration or the completion feedback loop (see
  [Magic wand](#magic-wand--built-then-retired)).

### Anchoring model

Comments anchor with a [W3C Web Annotation Data Model][wadm]
`TextQuoteSelector`, serialised inline on the comment row
(`CommentAnchor {exact, prefix, suffix}` in
`tools/boss/protocol/src/types/comment.rs`):

```json
{
  "type": "TextQuoteSelector",
  "exact": "the rendered markdown source already pushes commented spans",
  "prefix": "Each comment carries a `TextQuoteSelector`-shaped anchor. ",
  "suffix": " through to the macOS app via the existing subscription"
}
```

[wadm]: https://www.w3.org/TR/annotation-model/#text-quote-selector

The anchor's three fields are strings taken from the **rendered
plain-text projection** of the markdown, not the raw source: the user
selects on rendered text, so what they see is what gets stored.
Prefix/suffix are 64 characters each, trimmed at word boundaries —
enough to disambiguate in Boss-sized docs, short enough that
collisions on edited docs are rare.

**Resolution is engine-owned.** This is the largest divergence from
the original design, which had the _renderer_ re-resolve anchors on
every load and merely report orphans. As-built, the renderer sends the
doc's plain-text projection to the engine via the `CommentsResolve`
RPC, and the engine (`engine/core/src/work/comments_anchor.rs`, a pure
module) resolves every anchor, persists fuzzy re-anchors, and flips
orphan status itself. Rationale (per #915): the algorithmic core stays
authoritative and testable in the engine, while the renderer still
supplies the plain text — the engine never parses markdown. The
resolution ladder:

1. **Exact match** — search for `prefix + exact + suffix` verbatim;
   anchored if found exactly once.
2. **Fuzzy match** — sliding-window scoring using a self-contained
   **Sørensen–Dice character-bigram coefficient** (the doc originally
   pointed at `fastdiff`; Dice was chosen to avoid a new crate
   dependency). Accepted when the best window scores ≥0.8 _and_ the
   best non-overlapping runner-up scores <0.7 (uniqueness). A fuzzy
   hit re-extracts a fresh 64/exact/64 anchor from the matched text
   and persists it with `last_resolved_with = 'fuzzy'`, so the next
   load exact-matches and the sidebar can show a ⚠ re-anchored glyph.
3. **Orphan** — neither resolves: the comment's status flips to
   `orphaned`; it still appears in the sidebar with its original
   snippet but paints no highlight.

The 0.8 / 0.7 thresholds (starting values from
[Hypothes.is's re-anchoring work][hypo-anchor]) are tunable via the
`BOSS_COMMENT_FUZZY_SCORE` / `BOSS_COMMENT_FUZZY_SECOND_BEST`
environment variables — process-wide, not the per-product config knob
originally sketched; nothing has needed per-product tuning.

[hypo-anchor]: https://web.hypothes.is/blog/fuzzy-anchoring/

**Why not `RangeSelector` or DOM-path-style anchors.** Both bind to
the rendered structure, so any structural edit (a new heading, a split
paragraph) breaks them. A plain `TextQuoteSelector` is
content-addressed: it survives any edit that doesn't touch the
immediate text around the anchor.

### Engine schema

One table, `work_comments`
(`migrate_work_comments_table`, `engine/core/src/work/migrations_b.rs`):

```sql
CREATE TABLE work_comments (
  id                            TEXT PRIMARY KEY,  -- "comment_…"
  artifact_kind                 TEXT NOT NULL,     -- 'work_item' | 'pr_doc'
  artifact_id                   TEXT NOT NULL,     -- work item id, OR
                                                   -- "pr_doc:<repo>:<branch>:<path>"
  doc_version                   TEXT NOT NULL,     -- SHA-256 of the plain-text
                                                   -- projection at authoring time
  anchor_json                   TEXT NOT NULL,     -- {exact, prefix, suffix}
  body                          TEXT NOT NULL,
  author                        TEXT NOT NULL,
  status                        TEXT NOT NULL,
  status_actor                  TEXT,
  last_resolved_with            TEXT,              -- 'exact' | 'fuzzy' | 'orphan'
  plain_text_projection_version INTEGER NOT NULL DEFAULT 0,
  created_at                    TEXT NOT NULL,     -- ISO timestamps (not epoch ints)
  updated_at                    TEXT NOT NULL,
  dismissed_at                  TEXT
);
CREATE INDEX work_comments_by_artifact ON
  work_comments(artifact_kind, artifact_id, status);
```

Differences from the originally sketched DDL, all additive:

- `last_resolved_with` — drives the sidebar's fuzzy-re-anchor ⚠ glyph.
- `plain_text_projection_version` — originally listed as a Phase 2
  "worth spec'ing" risk mitigation; it shipped in Phase 2.
- Timestamps are ISO-8601 `TEXT`, matching the rest of the engine
  schema, rather than the sketched `INTEGER`.

**Status vocabulary (as evolved).** The original set was
`active | dismissed | orphaned | resolved`. As-built:

- `active`, `resolved`, `orphaned` work as designed; `orphaned` is
  written by the engine's resolver (see above), not reported by the
  renderer.
- **Soft-dismiss lands on `resolved`, not `dismissed`**: the
  `CommentsDismiss` RPC transitions to `resolved`, and `dismissed` is
  reserved for a future hard-dismiss that has never been needed.
- Phase 4 added a `dispatched` status (not in the original design) for
  comments handed to a chore; the retirement migration
  (`migrate_retire_magic_wand_dispatched_comments`) later removed it,
  retiring any stranded rows.
- The successor design added `in_revision`, `answering`, `answered`,
  and `awaiting_followup` for the classifier/answer-agent flow.

Work items themselves gained no columns; comments are strictly
auxiliary state, so PR detection, ready-to-spawn checks, and dispatch
flow were untouched by this project.

`WorkComment`, `CreateCommentInput`, `CommentResolution`, and
`ResolvedComment` live in `boss-protocol` (`types/comment.rs`), with
`WorkComment` on the repo's `bon::Builder` convention.

### Doc-version invariant

`doc_version` is a SHA-256 of the doc's plain-text projection at
authoring time; the renderer provides the plain text inline in the
create RPC so engine and renderer agree on the input. It served two
purposes as designed: the magic-wand apply CAS (mismatch → conflict
surface, never a silent overwrite — this worked, and the CAS pattern
carried into the successor design's revise flow) and an
anchor-resolution diagnostic. No general history of doc versions is
stored; the field is opaque and compared only for equality.

### RPCs and subscription topics

As-built RPC surface (`protocol/src/wire.rs`; handlers in
`engine/core/src/app/comments.rs`), all user-tier:

- `CommentsCreate` — creates an `active` comment.
- `CommentsList` — comments for an artifact; default excludes
  `resolved` and `dismissed`, but **orphans are always shown** so lost
  anchors stay visible.
- `CommentsResolve` — **not in the original design.** The renderer
  posts the doc's plain-text projection; the engine resolves every
  anchor (exact/fuzzy/orphan), persists re-anchors and orphan flips,
  and returns per-comment `CommentResolution`s with character offsets
  for highlight painting. This RPC subsumed both the planned
  renderer-side resolver callback _and_ the planned
  `comments_fetch_with_doc_version` convenience RPC, which was never
  built.
- `CommentsDismiss` — soft-dismiss (→ `resolved`).
- `CommentsSetStatus` — explicit transitions among
  `active`/`resolved`/`orphaned`.
- `CommentsUpdateAnchor` — manual anchor rewrite; largely superseded
  by `CommentsResolve` persisting fuzzy re-anchors itself.

The three magic-wand RPCs (`CommentsDispatchMagicWand`,
`CommentsApplyMagicWand`, `CommentsDiscardMagicWand` — the single
sketched RPC grew explicit apply/discard verbs during implementation)
existed from Phase 3 until the retirement removed them. The successor
design's RPCs (`CommentsReviseDoc`, answer-agent surface) are
documented there.

Subscription topic, exactly as designed
(invalidation-not-patch, helper `comment_topic()` in `wire.rs`):

- `comments.artifact.<artifact_kind>:<artifact_id>` — fires on any
  comment-row change; clients refetch via `CommentsList`. Publishing
  uses a light invalidation path that skips work-graph reconcile.

### Comments on PR-backed docs

`artifact_id = "pr_doc:<repo_remote_url>:<branch>:<path>"`, parsed by
right-splitting on `:` (SSH remote URLs contain colons).

**Migration when a doc graduates from work-item description to PR.**
`DesignDetector::on_design_pr_detected` (the `in_review` transition)
calls `migrate_work_item_comments_to_pr_doc`: every active
`work_item` comment for the task is _copied_ to a new row keyed to the
`pr_doc:*` artifact, and the original is soft-resolved (actor
`engine_design_detector`) so the trail is visible. The operation is
idempotent across repeated detector polls. One divergence from the
original plan: migrated comments are **not re-anchored at migration
time** — the engine can't render markdown to plain text, so the new
rows carry their old anchors and re-anchor naturally on the renderer's
next `CommentsResolve` load.

**Branch lifecycle.** The designed background sweep — transitioning
`pr_doc` comments to `orphaned` when their branch is deleted — was
never implemented; `orphaned` is only ever written by the anchor
resolver. Comments on dead branches simply become unreachable (their
viewer no longer opens). The successor design's reconciliation
hooks (resolve-on-merge via `mark_chore_pr_merged`) cover the merge
case; closed-without-merge remains an open gap tracked there.

### macOS app architecture

The module is `tools/boss/app-macos/Sources/Comments/`. The shipped
shape differs from the original component sketch in several ways worth
recording — most were deliberate refinements out of the #622
validation pass.

1. **`CommentLayer`** — `@MainActor ObservableObject` owning the
   comment list, popover state, resolution results, and flash state;
   applied via the `.withComments()` ViewModifier (one line per
   surface, as designed). Engine persistence flows through
   **`CommentWire.swift`** (Codable mirrors of the protocol types +
   `EngineClient` calls — the deferred half of #915).

2. **Selection capture** — the designed `SelectionTracker` over
   NSTextView-bridged callbacks never materialised. As-built the app
   probes for a selection non-destructively via an
   `NSUserInterfaceValidations` "copy" validation check, and captures
   the selected text by sending `copy:` to the first responder and
   reading the pasteboard — an explicit trade-off accepted in Phase 1
   (pasteboard clobber at explicit-click granularity) that the
   refinement pass kept, only making the probe non-destructive.

3. **Entry triggers** (#622, all added beyond the original design):
   typing any printable character while text is selected opens the
   form with that character seeded into the body; right-clicking a
   selection offers an "Add Comment" context menu; ⌘⇧K is the
   keyboard shortcut (⌘⇧M was taken by the Metrics panel).

4. **`CommentPopover`** — as-built a 320pt `.popover` anchored to a
   fixed point near the top-left of the content area, not to the
   selection's bounding rect as designed (Phase 1 had shipped a
   centered modal sheet; #622 moved to the popover). **Return
   submits, Shift+Return inserts a newline** via `CommentTextEditor`
   (an NSTextView wrapper distinguishing `insertNewline` from
   `insertNewlineIgnoringFieldEditor`) — the designed ⌘-return submit
   was judged too much friction. The designed author field never
   shipped; the selected-text echo in the form was removed once the
   in-doc highlight showed the target.

5. **Highlighting** — the designed Canvas-based
   `CommentHighlightOverlay` with an `NSLayoutManager` rect bridge
   was never built (the file exists but is vestigial). Instead,
   **`HighlightingMarkdownParser`** wraps the markdown parser and
   injects a yellow (0.45-opacity) background attribute on commented
   spans, located by substring search against the plain-text
   projection. Anchor offsets come from `CommentsResolve`.

6. **`CommentSidebar`** — fixed 280pt right panel, appearing only
   when the artifact has ≥1 comment. The designed manual show/hide
   toggle never shipped (the entry triggers made it unnecessary for
   authoring the first comment); the only toggle is **"Show
   resolved"** for the soft-dismiss history surface. Dismiss is an
   `xmark.circle` at the card's top-right (macOS convention), moved
   there in #622. Clicking a row **flashes the anchored span orange
   for ~900 ms** (`CommentLayer.jumpTo`); the designed bidirectional
   hover tint and scroll-to-anchor never shipped — precise
   scroll-to-glyph needs the `NSLayoutManager` bridge that was
   deferred and never picked up.

7. **Thread rendering** — `CommentThreadEntry.swift` renders the
   successor design's engine-authored thread entries (nudges, answers,
   operator follow-ups); documented there.

`MagicWandResultSheet` (side-by-side preview, Apply/Discard, conflict
banner) shipped in Phase 3 and was deleted with the wand.

### Magic wand — built, then retired

Both dispatch routes shipped as designed in outline, then the whole
mechanism was removed by
[`comment-triggered-document-revisions`](comment-triggered-document-revisions.md).
The as-built record, and why it died:

#### Engine-owned docs → specialised isolated Claude (#970)

- `engine/core/src/magic_wand.rs` (the doc's sketched
  `engine/src/magic_wand.rs` predated the engine's split into crates)
  made a one-shot, non-streaming `messages.create` call —
  `claude-sonnet-4-6`, `max_tokens` 8192, 120 s timeout — with **no
  tools and no system prompt** beyond the inlined instructions, using
  the same prompt shape this doc originally specified. The sandboxing
  argument held: no filesystem, no environment, no memory between
  invocations; worst case is garbage markdown caught by validation.
- Transport was a hand-rolled `reqwest` client (pattern copied from
  `pane_summary.rs`) — the doc's assumed `anthropic-sdk` crate did not
  exist, and this predated the `claude_client` extraction.
- Validation shipped with the designed values: hard-reject outside
  [0.25×, 4×] source length; hard-reject when >60% of lines changed;
  anchor-preservation _warning_ when the anchor text vanished **and**
  > 30% of lines changed (the 30% is an implementation-chosen
  > threshold).
- `BOSS_MAGIC_WAND_API_KEY` with `ANTHROPIC_API_KEY` fallback shipped
  exactly as designed; token counts recorded on
  `magic_wand_dispatches` (as-designed schema plus an
  `anchor_warning` column and, later, `chore_id`). The table survives
  today as an unread historical record.
- Dispatch was gated `RpcTier::AppOrBoss`; apply/discard were
  deliberately left user-tier.
- Apply ran the doc-version CAS exactly as designed: match →
  overwrite description, dispatch → `applied`, comment → `resolved`;
  mismatch → `conflict` + reload affordance. #970 attributed the
  apply to `"user"` rather than the designed
  `magic_wand:<comment_id>`; the follow-up fixing that (#1102) was
  still unmerged when the wand was retired.
- **The app-side dispatch button remained a stub** — #970 landed
  before the app's Phase 2 persistence wiring, so the engine-owned
  wand was never operable end-to-end from the UI.

#### PR-backed docs → Boss chore worker (#1106)

- The dispatch handler grew a `match` on `artifact_kind`; the
  `pr_doc` arm resolved the owning product from the repo remote URL
  and created a chore directly via `WorkDb::create_chore` (in-process,
  not the RPC round-trip the doc sketched), titled
  ``Address comment on `<path>`: `<short_quote>` `` with a directive
  embedding file, branch, quoted anchor, and comment body.
- The designed PR-resume integration did not exist yet (T520); the
  stopgap was steering the worker entirely through the directive
  ("push to the existing PR branch. Do not open a new PR").
- Attribution landed as chore provenance
  (`created_via = "comment_dispatch:<comment_id>"`) rather than an
  owner field. The comment moved to the new `dispatched` status at
  spawn time; the designed close of the loop — flipping to `resolved`
  when the worker finished — was never wired, so dispatched comments
  parked until the retirement migration cleaned them up.

#### Why it was retired

The wand assumed every comment wanted a mechanical edit. In practice
comments carry three intents — directives, questions, and
larger-change requests — and a single-comment auto-edit path serves
none of them well: questions get over-applied, batches get
under-specified. The successor design classifies every comment's
intent and routes directives/larger-changes into the existing
revision-task machinery (nudge, never auto-apply) and questions to a
read-only answer agent. `magic_wand.rs`, the three RPCs, and
`MagicWandResultSheet` are deleted; stranded `dispatched` comments
were retired by migration.

### Versioning

The narrow scope held: **comment-anchor CAS only**. Each comment
records the `doc_version` it was authored against; the apply step
compared it before overwriting. No history, no rollback, no diff-view
of past versions. Broader description history remains a separate
project (`Work-item description history`).

## Decisions as-built (formerly "open questions")

- **Prefix/suffix length**: 64 chars, word-boundary trimmed — shipped.
- **Fuzzy thresholds**: ≥0.8 best / <0.7 runner-up — shipped, tunable
  via env vars rather than per-product config.
- **Deleted anchor element**: orphan, shown in the sidebar with its
  original snippet, no highlight — shipped (engine flips the status).
  Manual re-attach remains unbuilt.
- **Sidebar**: 280pt fixed, right side — shipped. Appears only when
  comments exist; the explicit toggle was dropped in favour of the
  three authoring entry triggers. No auto-scroll; navigation is
  click-to-flash (hover linkage and scroll-to-anchor never shipped).
- **Threading**: single-level in this project; structured thread
  entries arrived with the successor design.
- **Cross-doc migration**: copy + soft-resolve originals on
  `in_review`, re-anchor on next load — shipped.
- **Magic-wand result UX**: side-by-side preview with explicit
  Apply/Discard — shipped, then deleted with the wand. The successor
  replaces preview-and-apply with nudge-toward-revision.
- **Streaming**: non-streaming, as recommended; never revisited before
  retirement.
- **Concurrent commenters / permissions**: as designed — separate
  rows, no locking; anyone can dismiss; `author`/`status_actor`
  recorded but unused by UI.

## Risks — what materialised

- **Renderer in the trust path for selectors**: mitigated as
  spec'd — `plain_text_projection_version` shipped in Phase 2.
- **Fuzzy false positives**: the `last_resolved_with = 'fuzzy'` marker
  and sidebar ⚠ glyph shipped as the designed mitigation.
- **Wand returning subtly broken markdown**: the human-in-the-loop
  preview was the checkpoint; the deeper problem turned out to be
  upstream — the wand couldn't tell whether an edit was wanted at all.
  This risk section under-scoped the real failure mode, which is what
  killed the feature.
- **Cost/budget on wand calls**: never became real before retirement;
  the separate-key bucketing shipped and the per-day cap was never
  needed.

## Related designs

- [`comment-triggered-document-revisions`](comment-triggered-document-revisions.md)
  — the successor: intent classification, revision nudges, the
  read-only answer agent, and the magic-wand retirement migration.
- [`revision-tasks`](revision-tasks.md) — the revision substrate the
  successor routes directive comments into.
- [`markdown-renderer-migration`](markdown-renderer-migration.md) —
  the renderer this overlay attaches to.
- [`design-producing-tasks`](design-producing-tasks.md) — design-doc
  lifecycle, including the `in_review` transition that triggers the
  work-item→pr_doc comment migration.
- [`project-design-doc-pointer`](project-design-doc-pointer.md) —
  how a project's design doc is located, used to resolve the artifact
  id for `pr_doc:*` comments.
- [`engine-app-rpc`](engine-app-rpc.md) — RPC conventions the
  `comments_*` calls follow.
- [`work-subscriptions`](work-subscriptions.md) — the topic shape
  `comments.artifact.*` follows.
