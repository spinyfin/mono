# Boss: Attentions

- **Status:** shipped (v1) — schema/protocol (#922), engine store + CRUD/RPC + events (#991), `ActionAttentionGroup` (#1075), creation pipeline (#1076), extraction backstops (#1386), `boss attention` CLI (#1105), Notifications toolbar + window (#1110), inline questions surface in the design-doc viewer (#1387).
- **Last design postmortem:** 2026-07-20 — this revision updates the doc to as-built reality.
- **Code:** `tools/boss/engine/core/src/work/attentions.rs` (store), `engine/core/src/attentions_detector.rs` (creation pipeline + backstops), `engine/core/src/app/attentions.rs` (RPC handlers), `protocol/src/types/attention.rs`, `cli/src/main.rs` (`boss attention`), `app-macos/Sources/AttentionsView.swift`, `app-macos/Sources/DesignRendererQuestionsPanel.swift`.
- **Follow-on design:** `notification-dedup-scoring.md` later layered scoring/merge columns (`score`, `merged_into_attention_id`, `linked_work_item_id`, the `attention_merges` table) and merge UI onto this store; those are documented there, not here.

## Problem

Agents working on Boss tasks routinely reach a point where they need the human. A design worker writing a doc has genuine open questions ("should this be one table or two?", "yes/no: do we gate extraction behind a flag?"). An implementation worker finishing a chore notices three follow-on pieces of work worth filing. Before this feature, none of that reached the operator as an _actionable_ signal. It lived in the transcript, or at best in an "Open questions" section of a design doc that nobody is paged about. The operator found out by reading, then had to hand-translate "the agent asked X" into a doc edit or a new task.

There _was_ an existing attention surface — `work_attention_items` (`attn_…` ids, the `work_attention_items` table) — but it is a fundamentally different thing: engine-raised **operational alerts** ("repo unresolved", "manifest missing", "CI budget exhausted", "parent PR merged mid-revision"). Those are conditions the system surfaces and that auto-resolve when the underlying state clears. They carry no question, no answer, no proposed work, and no grouping. They are not what an agent raises to _ask the human something_.

This doc designs **Attentions**: actionable notifications an agent raises to pull the human into the loop. Every attention always has an action attached — an attention with no possible action is not an attention. The two launch kinds are **Question** (the agent wants the human to answer something, typically feeding back into a design doc) and **Followup** (the agent proposes a piece of work it noticed while completing a task). Attentions surface in a **Notifications** toolbar window and, for questions about a design doc, **inline in the design-doc viewer**. Critically, related attentions **batch into groups** so that answering ten questions about one doc produces **one** revision, not ten.

## Goals

- A first-class **attention** concept: an agent-authored, human-actionable notification that always carries an action and is associated with a project **or** a task/chore (provenance + a jump target).
- **Two initial kinds, extensible schema**: `question` and `followup`, with room for more kinds without a schema break.
- **Question types** that shape how the human answers: `yes_no`, `multiple_choice`, `prompt` (free text) — represented in schema and rendered as the matching inline control.
- **Batching / grouping**: related attentions collect into a **group** keyed by `(kind, association, source)`; the group is the unit the human acts on, and actioning a group produces a **single** downstream artifact (one revision for a question group; one batch task-create gesture for a followup group) — never N artifacts for N members.
- A **creation pipeline owned by the engine**: structured emission by the agent is the primary path (design-doc question manifests; end-of-task followup blocks), with post-hoc transcript/doc extraction as a backstop. The UI is a thin client.
- A **clear state machine** with defined "take action" semantics per kind and how the action closes the attention.
- A **`boss attention` CLI** mirroring existing conventions (`--json` envelopes, `--no-input`, `--product`, `T`/`P` selectors), covering create / list / show / answer / action / dismiss.
- **Two app surfaces**: a Notifications toolbar item opening an attentions window, and an inline questions surface in the design-doc viewer. Answering in either surface drives the identical downstream effect.
- Reuse of the existing **`boss task create-revision`** path for question groups whose source design doc still has an open PR.

## Non-Goals

- **Folding `work_attention_items` into this store.** Operational alerts stay where they are and keep their own lifecycle. The new Notifications window may _present_ both later behind a category filter, but the stores stay separate (see Alternatives). Renaming/migrating the legacy table is explicitly out of scope.
- **Blocking a worker on an answer.** When an agent raises a mid-task question, the task does not halt waiting for a human. The agent proceeds with its best judgement; the answer feeds a _later_ revision. Synchronous "agent waits for human" is a future concern.
- **Building the GitHub PR-review-comment triage UI.** That effort (noted in the revision-tasks design) is a separate producer that may _create_ attentions later; this design must not preclude it, but does not build it.
- **A general inbox / chat with the agent.** Attentions are discrete, typed, actionable items — not a conversation thread.
- **Auto-applying answers without producing a reviewable artifact.** Answering a question never silently rewrites a merged doc; it spawns a revision/design task whose output is a PR the human reviews and merges.
- **Auto-accepting followups.** A proposed followup is never turned into a task without an explicit human gesture.
- **Cross-product attentions.** An attention belongs to exactly one product, inherited from its association.
- **New friendly-id scheme beyond a per-product `A<n>` for groups.** Individual member attentions are referenced by primary id in the CLI (like `cir_…` ci-remediation ids today); only the actionable _group_ earns a short id.
- **Auto-creating projects from followups (v1).** A followup carrying `proposed_work_kind = "project"` is materialized as a plain task in the originating project rather than spawning a project — a deliberate v1 simplification made in #1075.

## Alternatives considered

### A. Extend `work_attention_items` with question/answer/followup columns

Reuse the existing `attn_…` table: add `question_type`, `answer`, `choice_options`, `proposed_*`, and a `group_id`, and discriminate via new `kind` values (`question`, `followup`).

**Rejected.** `work_attention_items` is read by the engine's _operational_ loop — a `repo_unresolved` row gates dispatch; rows auto-resolve when the condition clears. Mixing human-answered questions into that table risks engine code treating a question as a dispatch blocker (or auto-resolving a question because some unrelated state changed). The shapes barely overlap: operational alerts have no answer payload, no question type, no grouping, no proposed-work payload, and no notion of "actioning produces a revision". The only genuine overlap is the _UI list_, which we get by giving both stores a common list-presentation shape (below) — not by sharing a table. Keeping a clean store also lets the operational table be renamed later without disturbing this feature.

### B. Pure post-hoc extraction as the primary creation path

Don't change agent instructions at all. Run a supervisor model pass over every completed transcript and every design doc to extract questions and followups, and turn whatever it finds into attentions.

**Rejected as the primary path** (kept as a backstop). Extraction is brittle and lossy for exactly the data we most need: it can't reliably tell a yes/no question from a multiple-choice one, can't recover the _choices_, and can't anchor a question to the doc section it's about (which the inline surface needs). It can also hallucinate questions or work that the agent never actually raised. Structured emission — the agent telling us, in a fenced manifest, exactly `{type, prompt, choices, anchor}` — is precise, cheap (no extra model call), idempotent, and reuses machinery we already have for design-producing tasks (the `DOC_REF:` sentinel + sibling-manifest pattern). We therefore **lead with structured emission and fall back to extraction**, flagging extracted rows as lower-confidence. (As built, the cost asymmetry turned out milder than feared: the questions backstop is pure markdown parsing with no model call at all; only the followups backstop pays for a model call.)

### C. One attention = one downstream artifact (no grouping)

Each question answered immediately spawns its own revision; each followup immediately becomes a task on accept.

**Rejected.** This is precisely the failure the brief calls out: a design worker that emits ten questions would spawn ten revision tasks against one doc, each opening or amending a PR, each needing its own review. Grouping is load-bearing, not a nicety. The group — not the member — is the unit that produces an artifact.

### D. Engine edits the doc directly on answer (no revision worker)

When the human answers, the engine itself rewrites the markdown and commits it.

**Rejected.** It loses the agentic reconciliation that makes the answer _land well_ (the answer "yes, two tables" needs a worker to actually restructure the schema section, not a string splice). It can't handle the "doc already merged → needs a fresh PR" case, bypasses review, and fights the PR-branch model. Spawning a revision (open PR) or a fresh design task (merged doc) reuses the pipeline we already trust and keeps a human in the merge loop.

## Chosen approach (as built)

An **attention** is a single agent-authored, actionable notification. Attentions never stand alone in the UI — they belong to an **attention group**, the unit the human reads and acts on. The engine owns creation, reconciliation, state transitions, and producing downstream artifacts; the macOS app and the CLI are thin clients over engine RPC.

### Data model

Two tables plus a short-id sequence table, added via the idempotent `migrate_attentions` (`engine/core/src/work/migrations_b.rs`, schema v13). The tables shipped column-for-column as designed; the deltas are noted after each table.

**`attention_groups`** — the actionable unit. Id prefix `atg` (generated via the engine's `next_id("atg")`).

| column                       | type             | notes                                                                        |
| ---------------------------- | ---------------- | ---------------------------------------------------------------------------- |
| `id`                         | TEXT PK          | `atg_{nanos:x}_{counter:x}`                                                  |
| `product_id`                 | TEXT NOT NULL    | FK `products(id)`; inherited from association                                |
| `short_id`                   | INTEGER NULL     | per-product `A<n>` friendly id (see allocation note below)                   |
| `kind`                       | TEXT NOT NULL    | `question` \| `followup` (extensible)                                        |
| `association_project_id`     | TEXT NULL        | FK `projects(id)`                                                            |
| `association_task_id`        | TEXT NULL        | FK `tasks(id)`                                                               |
| `source_kind`                | TEXT NOT NULL    | `design_doc` \| `task_transcript` \| `manual`                                |
| `source_task_id`             | TEXT NULL        | originating design/impl task (the jump-back target)                          |
| `source_run_id`              | TEXT NULL        | transcript pointer (`runs.id`); pairs with `runs.transcript_path`            |
| `source_doc_path`            | TEXT NULL        | repo-relative design-doc path (for `design_doc`)                             |
| `source_doc_repo_remote_url` | TEXT NULL        | canonical repo form                                                          |
| `source_doc_branch`          | TEXT NULL        | head branch for in-review viewing                                            |
| `grouping_key`               | TEXT NOT NULL    | derived stable key (below); upsert dedup target                              |
| `generation`                 | INTEGER NOT NULL | bump per source re-run so a new run never merges into a closed group         |
| `state`                      | TEXT NOT NULL    | `open` \| `partially_answered` \| `actioned` \| `dismissed` (default `open`) |
| `produced_artifact_kind`     | TEXT NULL        | `revision` \| `design_task` \| `tasks` (set on action)                       |
| `produced_artifact_ref`      | TEXT NULL        | JSON: `{"task_id","short_id"}` or `{"tasks":[{task_id,short_id,kind}…]}`     |
| `created_at`                 | TEXT NOT NULL    | RFC 3339 / epoch seconds, repo convention                                    |
| `actioned_at`                | TEXT NULL        |                                                                              |
| `dismissed_at`               | TEXT NULL        |                                                                              |

XOR CHECK: exactly one of `association_project_id` / `association_task_id` is non-null (mirrors the `work_attention_items` CHECK). Unique index on `(grouping_key, generation)` makes reconciliation an upsert (and arbitrates the concurrent-create race: the loser errors on the index and retries). A partial-unique index on `(product_id, short_id)` guards the friendly ids, and an extra `(product_id, state, created_at)` index serves the default list query.

**Short-id allocation diverged from the original plan.** The doc originally called for the tasks/projects partial-unique-index pattern alone; what shipped is a dedicated dense per-product counter table, `attention_group_short_id_sequences (product_id PK, next_value)`, copying the automations pattern, allocated in `dispatch_helpers.rs`. This yields dense `A1, A2, …` per product — but the `A<n>` display namespace visually collides with automations' `A<n>` (separate counters), and RPC-level `A<n>` resolution is cross-product (the request carries no product id), erroring on ambiguity. See open items.

**`attentions`** — a single member of a group. Id prefix `atn` (`next_id("atn")`). (Distinct from the legacy operational `attn_…` ids; the extra `t` is the only visible difference, so implementation treats the prefix as a hint only and never keys logic on it. A later cleanup may rename the operational prefix — still out of scope.)

| column                 | type             | notes                                                                                     |
| ---------------------- | ---------------- | ----------------------------------------------------------------------------------------- |
| `id`                   | TEXT PK          | `atn_{nanos:x}_{counter:x}`                                                               |
| `group_id`             | TEXT NOT NULL    | FK `attention_groups(id)` ON DELETE CASCADE; index `(group_id, ordinal)`                  |
| `ordinal`              | INTEGER NOT NULL | display order within the group                                                            |
| `source_anchor`        | TEXT NULL        | doc section / heading slug (questions) or transcript offset hint; drives inline placement |
| `answer_state`         | TEXT NOT NULL    | `open` \| `answered` \| `skipped` \| `dismissed` (default `open`)                         |
| `created_at`           | TEXT NOT NULL    |                                                                                           |
| `answered_at`          | TEXT NULL        |                                                                                           |
| **question fields**    |                  | populated when `group.kind = question`                                                    |
| `question_type`        | TEXT NULL        | `yes_no` \| `multiple_choice` \| `prompt`                                                 |
| `prompt_text`          | TEXT NULL        | the question shown to the human                                                           |
| `choice_options`       | TEXT NULL        | JSON array of strings (for `multiple_choice`)                                             |
| `answer`               | TEXT NULL        | captured answer: `"yes"`/`"no"`, chosen index/value, or free text                         |
| **followup fields**    |                  | populated when `group.kind = followup`                                                    |
| `proposed_name`        | TEXT NULL        | pre-fills task name                                                                       |
| `proposed_description` | TEXT NULL        | pre-fills task description                                                                |
| `proposed_effort`      | TEXT NULL        | effort hint (`trivial`…`max`)                                                             |
| `proposed_work_kind`   | TEXT NULL        | `task` \| `chore` \| `project` (`project` materializes as a task in v1)                   |
| `rationale`            | TEXT NULL        | why the agent suggested it                                                                |
| `confidence_source`    | TEXT NOT NULL    | `structured` \| `extracted` (provenance / trust flag); DDL default `'structured'`         |

The Rust `AttentionGroup`, `Attention`, and `CreateAttentionInput` structs live in `protocol/src/types/attention.rs` and follow the repo's builder convention — `#[derive(bon::Builder)]` with `#[builder(on(String, into))]`, `Option<T>` fields auto-optional, `#[builder(default = …)]` for `state`/`answer_state`/`generation`/`confidence_source`. The production DB mappers (`map_attention`, `map_attention_group` in `work/mappers.rs`) use struct literals so a new column is a compile error until mapped, per the repo convention. (The later dedup-scoring feature added `score`, `merged_into_attention_id`, `linked_work_item_id` columns and the `attention_merges` table on top of this schema; see that design.)

### Grouping model and partial-answer semantics

The **grouping key** is the stable string `kind|association|source-discriminator`, derived by `derive_grouping_key`:

- **Questions from a design doc**: `question|{project_id}|doc:{source_doc_path}`. All questions a worker raises about one doc collapse into one group.
- **Followups from a task transcript**: `followup|{originating_task_id}` (`source_task_id`, falling back to `association_task_id`). All followups a worker proposes while completing one task collapse into one group.

`generation` separates re-runs: reconciliation targets the latest-generation group for the key when it is still open/partially answered; if that group is already `actioned`/`dismissed`, a fresh group is inserted at `generation + 1`. This is what keeps "one group ⇒ one revision" true across iteration.

**Member-level content dedup (added during implementation, #1076).** Beyond the `(grouping_key, generation)` upsert the doc planned, reconciliation also derives a per-member `content_key` (`question_type` + `prompt_text` + `source_anchor` for questions; `proposed_name` for followups) so a re-detected PR or re-emitted followups block never appends duplicate members. The same prompt at two different anchors stays two distinct members.

**Partial answers (multi-sitting).** Members carry their own `answer_state`. A human can answer 3 of 10 questions now and the rest later; each `answer`/`answer_state` is persisted independently and the group sits at `partially_answered`. Member mutation precedence is `dismiss > skip > answer`; skipping or dismissing nulls out any captured answer; answering a `question` requires a non-empty value while accepting a `followup` does not; terminal groups reject member mutation. Nothing downstream happens until the human **actions** the group.

**Actioning a group is a single, terminal gesture.** At action time the engine requires every member to be in a terminal answer-state — `answered`, `skipped`, or `dismissed` — with a `skip_unanswered` option that bulk-skips the remainder so the human isn't forced to touch every row. A group whose terminal members include zero `answered` (questions) or zero accepted (followups) is refused with "dismiss it instead" — actioning always produces something. Actioning then:

1. produces **one** downstream artifact from the `answered` set (skipped/dismissed members contribute nothing),
2. records `produced_artifact_kind` + `produced_artifact_ref` on the group,
3. transitions the group to `actioned` (terminal) — all in one transaction with the artifact insert, so a re-action can never double-produce.

If, later, the source emits _new_ questions/followups, they land in a **new** group (next `generation`) — they never reopen a closed one. This is the mechanism that prevents the N-revisions explosion while still letting iteration continue.

### Creation pipeline (engine-owned, structured-first hybrid)

The engine creates attentions; agents and the CLI are producers, never the source of truth for grouping/reconciliation. All paths funnel through `WorkDb::reconcile_attentions`, which applies the group upsert and member content dedup above. The pipeline lives in `engine/core/src/attentions_detector.rs`, invoked from the completion path.

**Questions — primary: structured emission from design docs.** The design-doc worker prompt (`compose_design_directive` in `runner.rs`) instructs workers to emit, alongside the doc and its `DOC_REF:` sentinel, a sibling **questions manifest** at `<slug>.attentions.json` — a JSON array of `{ question_type, prompt, choices?, anchor }`, where `anchor` is the heading slug the question pertains to. Rather than literally extending `DesignDetector`, `reconcile_design_doc_questions` runs as a sibling step immediately after the design detector's detected/merged transitions, gated on design-family task kinds; it re-scans the PR, fetches the manifest raw off the PR head branch while the PR is open (base branch once merged, trying both) via `gh api …/contents`, and reconciles. Malformed entries (unknown `question_type`, empty prompt, `multiple_choice` without choices) are dropped individually rather than failing the batch.

**Followups — primary: engine-owned structured output (evolved post-#1076).** As originally shipped, the primary path was a `FOLLOWUPS:` sentinel followed by a JSON array near the end of the transcript — parsed from assistant-authored text only (so the prompt's own instructions can't be mis-parsed), using the last sentinel, fenced or unfenced. A later hardening pass (#1386-era follow-on work) promoted an **engine-owned structured-output artifact** — a schema-validated `FollowupEntry` JSON file written outside the repo, like other worker structured outputs — to primary, demoting the transcript sentinel to fallback. Entries carry `{ proposed_name, proposed_description, proposed_effort?, proposed_work_kind?, rationale }`. `design_postmortem` executions are excluded from this generic path; they have their own mandatory postmortem-followups channel.

**Backstops — flag-gated extraction (#1386).** Two independent feature flags, both **default off**, category `attentions`:

- `attentions_questions_backstop`: when a design-doc PR ships no manifest, a **pure markdown parse** (no model call — cheaper than the design anticipated) of the doc's "Risks / open questions" section synthesizes `prompt`-type attentions, capped at 20, coarse-anchored to the fixed slug `risks-open-questions`.
- `attentions_followups_backstop`: when a completing worker emitted no structured followups, the last ~8,000 chars of assistant transcript text go to a cheap supervisor model (Haiku, direct Anthropic API call; requires `BOSS_BACKSTOP_API_KEY` or `ANTHROPIC_API_KEY`, silently no-ops without one) to extract candidates.

Both backstops flag rows `confidence_source = "extracted"` and run only when the corresponding primary path produced nothing.

> Justification, restated: structured emission is robust (exact type/choices/anchor), cheap, and idempotent, and rides infrastructure we already have (sentinels, sibling manifests, structured outputs, transcript-tail detectors). Extraction is flexible (works on anything) but lower-trust, so it is strictly a graceful-degradation backstop. Both paths write the same rows, so the UI never has to know which path produced an attention beyond the `confidence_source` flag.

A third path, **explicit/manual emission**, lets a worker raise an urgent mid-task question without waiting for transcript post-processing, via `boss attention create` (below). This writes through the same engine RPC and the same reconciliation.

### Engine behaviour and "take action" per kind

All state transitions go through one engine entry point, `ActionAttentionGroup` (`WorkDb::action_attention_group`), so the toolbar window, the inline doc surface, and the CLI produce identical effects.

**Question group → a doc revision.** On action, the engine gathers the `answered` members into a markdown **Q&A brief** (question, captured answer, `§ anchor` per entry) and:

- **Attempts a revision first**: via the existing create-revision gate (`assert_parent_revisable_and_insert`), parented to `group.source_task_id`, probing the parent PR's live state. If the gate admits (design doc still in review), the revision worker is handed the brief and pushes to the existing PR branch — no new PR. `produced_artifact_kind = revision`.
- **Falls back to a fresh `design`-kind task** when the revision gate refuses (typically: the doc's PR already merged) or when the group has no `source_task_id` at all — the fresh task targets the same project, seeded with the brief, and opens a new PR. `produced_artifact_kind = design_task`.

Note the as-built fork triggers on _gate refusal_, not on an explicit "is the doc merged" check — same outcome, but a `gh` probe failure surfaces as an error rather than silently falling back. Either way the group flips to `actioned` and records the produced task so the card can link straight to it.

**Followup group → batch task-create.** On action, the human has marked each member `answered` (accept) or `skipped`/`dismissed` (reject). The engine creates the accepted members in one transaction — one `insert_task_in_tx`/`insert_chore_in_tx` per member (with the duplicate guard bypassed), rather than the `CreateTask`/`CreateMany` RPCs the doc originally named. A member becomes a **chore** when `proposed_work_kind = "chore"` _or_ when the originating item has no project; a `"project"` hint becomes a plain task (v1 simplification, see Non-Goals). One human gesture, 0..N work items, one group closure. `produced_artifact_kind = tasks`.

All artifacts produced by actioning — revision, design task, and followup tasks/chores alike — are stamped `created_via = "attention"` (a new `KNOWN_CREATED_VIA` value, chosen over reusing `engine_auto`).

**Dismiss.** A whole group or a single member can be dismissed without producing anything (`state = dismissed` / member `answer_state = dismissed`). The CLI's `--reason` is accepted for interface parity but not persisted — there is no reason column.

**Live updates.** The engine pushes events over the existing frontend socket (the same mechanism `AttentionItemCreated` uses): `AttentionCreated` per new member on creation, `AttentionGroupUpdated` on answer/dismiss, and `AttentionGroupActioned { group, members }` on action, so both app surfaces live-update without polling. The reply events `AttentionGroupsList` / `AttentionGroupResult` carry full member lists (`members: Vec<Attention>`, added in #1110 when the app needed them — the original wire shape was groups-only).

### CLI surface

A top-level noun `boss attention` under the existing clap command tree (`Commands::Attention { command: AttentionCommand }`), matching `task`/`project`/`chore`. All six verbs shipped in #1105. Verbs honour the global `--json`, `--no-input`, `--quiet`, `--product` flags and resolve `T`/`P` selectors via the existing selector machinery; group selectors accept `A<n>` or `atg_…`.

```
boss attention list                       # groups for the resolved product
  [--product <slug|id>]
  [--project P12 | --task T34]             # filter by association
  [--kind question|followup]
  [--state open|partially_answered|actioned|dismissed]   # default: open + partially_answered
  [--members]                              # RESERVED — currently a no-op (see open items)
  [--json]                                 # -> { "attention_groups": [...] }

boss attention show <A12|atg_…> [--json]   # group only today; member output is an open item

boss attention create --kind question --question-type yes_no|multiple_choice|prompt \
  --prompt "…" [--choice "A" --choice "B" …] \
  (--project P12 | --task T34) \
  [--group <A12|atg_…> | --group-key <key>]   # join an open group; else engine derives
boss attention create --kind followup \
  --name "…" --description "…" [--effort small] [--work-kind task|chore|project] \
  [--rationale "…"] \
  (--project P12 | --task T34)
  # --json -> { "attention": …, "attention_group": … }

boss attention answer <atn_…> \
  ( --yes | --no                # yes_no
  | --choice <index|value>      # multiple_choice
  | --answer "…" )              # prompt
boss attention answer <atn_…> --skip            # mark skipped
boss attention dismiss <A12|atg_… | atn_…> [--reason "…"]   # reason not persisted

boss attention action <A12|atg_…> [--skip-unanswered] [--confirm]
  # finalize: questions -> one revision/design task; followups -> batch create.
  # --json returns { "attention_group": <group>, "produced": { "kind": …, "ref": … } }
```

Caveats discovered in use: `A<n>` resolution only searches **active** (open / partially answered) groups — an actioned or dismissed group must be referenced by its `atg_…` id — and, at the RPC layer, `A<n>` lookup is cross-product with an ambiguity error, since the request carries no product id.

**Who creates attentions via the CLI?** The dominant path is engine-side (manifests + structured outputs + extraction during completion processing) and does **not** round-trip through the CLI — the engine writes the store directly. `boss attention create` exists for the explicit mid-task emission case and for tooling/tests; it is a thin RPC client like every other verb. Agents are _encouraged_ to use the structured manifest/sentinel emission over imperative `create` calls, because the manifest path is reconciled idempotently and survives re-runs, whereas a bare `create` is a one-shot.

The RPC variants mirror the existing `*AttentionItem*` ones in `wire.rs`: `ListAttentionGroups`, `GetAttentionGroup`, `CreateAttention`, `AnswerAttention`, `ActionAttentionGroup`, `DismissAttention`; push events `AttentionCreated`, `AttentionGroupUpdated`, `AttentionGroupActioned`, plus the reply envelopes `AttentionGroupsList` and `AttentionGroupResult`.

### App UI

**Notifications toolbar item.** A `ToolbarItem(placement: .primaryAction)` in `ContentView`'s toolbar (`NotificationsToolbarButton`) — a bell glyph with a hand-rolled red count capsule overlaid top-trailing (SwiftUI's `.badge` only applies inside `List`/`TabView`, so the doc's original `.badge(openGroupCount)` plan didn't survive contact; the overlay hides at 0 and caps at "99+"). Clicking opens the **Attentions window** — shipped as a **singleton `Window("Notifications", id: "attentions")`** scene rather than the value-keyed `WindowGroup` originally sketched, on the rationale that there is one product-scoped notifications surface, not many. A future iteration may demote it to a popover/panel anchored on the toolbar item — flagged, not built.

**Attentions window — grouped cards** (`AttentionsView.swift`). The list is grouped by `attention_groups`. Each **group card** shows: kind chip, association jump links (task/project and, for questions, the design doc — opens the existing `DesignRendererView` window), a one-line source summary, member count, an `extracted` badge when any member came from a backstop, and the kind-appropriate bulk action. Expanding shows member rows with inline controls:

- **Question / `yes_no`** → a segmented — / Yes / No control.
- **Question / `multiple_choice`** → a radio list built from `choice_options`.
- **Question / `prompt`** → a multi-line text field with Save + Skip.
- **Followup** → proposed name + description + effort/work-kind chips with an Accept / Reject toggle.

Per-member answers persist independently via `AnswerAttention` (the multi-sitting flow). The card footer is a single **Submit answers** button that flushes drafts and calls `ActionAttentionGroup` with `skip_unanswered` — the "skip remaining" affordance. A **"Recently resolved"** section (added beyond the original sketch) keeps just-actioned groups visible with jump links to the produced revision/tasks. **Known regression:** the followup-side footer (**Create selected**) and the per-group **Dismiss** button that #1110 shipped were dropped in a later refactor (the dedup-scoring work), leaving followup groups with no action affordance in the window — see open items.

**Inline in the design-doc viewer** (`DesignRendererQuestionsPanel.swift`, #1387). `DesignRendererView` gains a **collapsible 320pt right sidebar** (sidebar chosen over the bottom-bar alternative) listing the open question group(s) whose `source_doc_path` suffix-matches the doc on screen (matching is cross-product), reusing the _same_ group cards and inline controls as the window — so a doc revision produced inline is indistinguishable from one produced in the Notifications window. Each question shows its `§ <anchor>` slug as a label (in both surfaces). **What did not ship:** the anchored placement the design called for — questions rendered next to the section they concern via the comments-in-markdown text-anchor substrate — was deferred pending that substrate (P529) and remains unbuilt even though P529 has since landed; the sidebar is a flat list today. See open items.

**Grouped everywhere** so a batch of questions is always answered together and yields one revision.

### Schema and wire summary (as built)

- Tables `attention_groups` (`atg_…`, per-product `A<n>` short id via `attention_group_short_id_sequences`) and `attentions` (`atn_…`), added via idempotent `migrate_attentions` (schema v13). XOR CHECK on the group's association; unique `(grouping_key, generation)`; partial-unique short-id index plus a `(product_id, state, created_at)` list index; FK `attentions.group_id → attention_groups.id` ON DELETE CASCADE.
- Protocol structs `AttentionGroup`, `Attention`, `CreateAttentionInput` in `protocol/src/types/attention.rs` (builder pattern, serde conventions; enums serialized lowercase/snake_case like `EffortLevel`).
- `FrontendRequest` variants `ListAttentionGroups`, `GetAttentionGroup`, `CreateAttention`, `AnswerAttention`, `ActionAttentionGroup`, `DismissAttention`; `FrontendEvent` variants `AttentionGroupsList` and `AttentionGroupResult` (reply envelopes, with members) plus push events `AttentionCreated`, `AttentionGroupUpdated`, `AttentionGroupActioned`.
- CLI noun `boss attention` with list / show / create / answer / dismiss / action.
- Engine: `attentions_detector.rs` (question-manifest reconcile as a sibling of the design detector; structured-output-first followups reconcile; two flag-gated extraction backstops), `work/attentions.rs` (store, grouping, `action_attention_group`), `app/attentions.rs` (RPC handlers + event pushes).
- App: Notifications toolbar bell + count overlay, singleton Attentions window with grouped cards, and a flat questions sidebar in `DesignRendererView`.

## Resolved questions

The original review questions (OQ1–OQ9), with their as-built outcomes:

1. **"Take action" + state transition (OQ1) — confirmed and shipped as designed.** Question group → one revision (open PR) or one fresh `design` task (merged doc / no source task); followup group → batch task-create. `actioned` is terminal; new emissions form a new generation.

2. **Grouping key + partial answers (OQ2) — confirmed and shipped**, with the addition of member-level `content_key` dedup. The "skip remaining" bulk step shipped as `skip_unanswered` and is the window's submit behaviour.

3. **Structured vs. extraction (OQ3) — hybrid shipped, structured-first**, and the structured side got _stronger_ post-launch: the followups primary path moved from a transcript sentinel to an engine-owned schema-validated structured-output artifact, with the sentinel as fallback and extraction as the flag-gated last resort.

4. **Answer → concrete doc revision (OQ4) — confirmed.** Open-PR docs reuse the create-revision path; merged docs get a fresh seeded `design` task. The fork is implemented as revision-gate refusal → fallback rather than an explicit merge check.

5. **Relationship to existing surfaces (OQ5) — confirmed.** Separate store from `work_attention_items`; the `atn`/`attn` prefix proximity was accepted and has caused no observed confusion; no unified table.

6. **Short-id namespace (OQ8) — shipped, with rough edges.** Groups got `A<n>` via a dense per-product sequence table (automations pattern) rather than index-max allocation; members have no friendly id, referenced by `atn_…`. Remaining rough edges: the `A<n>` display namespace visually collides with automations' `A<n>`, and resolution is cross-product and active-groups-only (see open items).

7. **Mid-task questions non-blocking (OQ9) — confirmed**, unchanged: `boss attention create` never blocks the worker; answers land in a later revision.

## Open items (as of the 2026-07-20 postmortem)

- **Anchored inline placement never shipped.** Questions in the design-doc viewer are a flat sidebar; the `source_anchor` is shown only as a `§ slug` label. The blocking substrate (P529 comments anchoring) has since landed in `DesignRendererView`, so the deferred integration — rendering questions next to the heading they concern, degrading to the flat list when an anchor doesn't resolve — is now unblocked but undone. The original anchor-drift risk (headings moving across revisions) therefore remains unvalidated.
- **Followup groups lost their action affordance in the app.** A post-#1110 refactor removed the "Create selected" footer and the per-group Dismiss button from `AttentionsView`; followup groups can currently only be actioned via `boss attention action`. The view-model's dismiss methods are live but uncalled.
- **CLI member output is stubbed.** `boss attention show` prints the group only and `--members` is a declared no-op, with help text claiming the protocol lacks member data — which has been false since #1110 added `members` to the reply events. The CLI just discards them.
- **No confirmation gate on extracted followups.** OQ7's "consider requiring human confirmation before an extracted followup can be actioned" was not built: `action_attention_group` never reads `confidence_source`, so an `extracted` (model-inferred) followup actions into real tasks exactly like a `structured` one. Both backstop flags default off, which bounds the risk for now; revisit before enabling them broadly.
- **`A<n>` ergonomics.** Cross-product resolution with an ambiguity error (the RPC carries no product id), active-groups-only short-id lookup, and the visual collision with automations' `A<n>` are all live rough edges; none blocks use.
- **Dismiss reasons are dropped.** `--reason` is accepted for parity and discarded; add a column or remove the flag if reasons ever matter.
