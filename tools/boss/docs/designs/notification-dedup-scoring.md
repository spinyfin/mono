# Boss: Notification Near-Duplicate Reconciliation + Scoring

## Problem

When several agents independently flag the same thing, each one creates its own notification. The user sees three cards that are really one concern, and the fact that *three* agents converged on it — the strongest priority signal we have — is buried as noise instead of surfaced as importance.

Boss already has the notification substrate. The agent-authored, human-actionable notification feature (design: [`attentions`](attentions.md)) is the **AttentionGroup** + **Attention** pair surfaced in the macOS "Notifications" toolbar window. An `AttentionGroup` is one card; its `Attention` members are the questions / followups inside it. (This is distinct from `WorkAttentionItem`, the legacy *engine-raised operational* alert store tied to executions — that store is **out of scope** here; see Non-goals.)

That substrate already dedupes **exact** matches at two granularities:

- **Group-level**, via `grouping_key` + `generation`: `resolve_or_create_group` joins a new attention into the latest open / partially-answered group for its grouping key (`engine/core/src/work/attentions.rs`), and a `(grouping_key, generation)` unique index makes the group idempotent.
- **Member-level**, via `content_key`: `reconcile_attentions` skips members whose `(kind, question_type, prompt_text, source_anchor, proposed_name)` tuple already exists in the group (`engine/core/src/work/attentions.rs`), so re-running the same source is a no-op.

What it does **not** catch is the case this project targets: two agents flag *the same concern* but compute *different grouping keys* (different source runs, different phrasings, different anchors). Exact-match dedup can't see that "The migration in `schema_init.rs` is missing an index on `created_at`" and "Add an index for the attention-items query — full scans on startup" are the same thing. Today those become two separate cards. We want an LLM to recognize the near-duplicate, fold it into one **canonical** notification, **increment a score** on that canonical card each time it happens, and surface the score as a priority affordance — all behind a feature flag, off by default.

### What is already built vs. what this adds

| Already implemented | This project adds |
|---|---|
| `AttentionGroup` / `Attention` data model + `attention_groups` / `attentions` tables | A `score` column on `attention_groups` + an `attention_merges` provenance ledger |
| Exact group dedup (`grouping_key` + `generation` unique index) | A semantic **near-duplicate** decision (LLM) layered *on top of* exact dedup |
| Exact member dedup (`content_key` in `reconcile_attentions`) | Creation-time redirect: fold a near-dup candidate into the canonical group instead of creating a new one |
| Anthropic API substrate (`pane_summary::claude_short_summary`, `live_status::SummarizerOutcome`) | A structured-output **dedup decision** call reusing that substrate |
| `merge_poller` background-sweep pattern (`run_one_pass`) | A bounded, idempotent **startup sweep** over existing groups |
| `AttentionsView` Notifications window | A score/priority affordance + score-aware ordering |
| Env-var config (`WorkConfig::load_from_env`) | A `notification_dedup_enabled` flag (`BOSS_NOTIFICATION_DEDUP`), off-safe default |

## Naming

- **Notification** — in this doc, an `AttentionGroup` (one card in the Notifications window). Its content is carried by its `Attention` members. The unit of dedup and the carrier of the score.
- **Candidate** — a notification about to be created (creation-time path) or an existing notification being re-examined (sweep path).
- **Canonical** — the surviving notification a duplicate is folded into. Its score is incremented; it may receive bounded minor edits.
- **Dedup decision** — the LLM transform: `(candidate, comparison set) -> is-duplicate? which canonical? proposed minor edits?`.
- **Score** — an integer on each notification: the number of independent reports folded into it. A fresh notification has score `1`. Each fold increments it.
- **Fold** — the act of reconciling a duplicate into a canonical: `score += 1`, write an `attention_merges` row, optionally apply bounded edits, and (sweep path) retire the loser.
- **`attention_merges`** — the durable provenance ledger; one row per fold. Records who was folded into whom, the model, the decision rationale, and any edits applied. Also the sweep's idempotency key.

---

## Goals

- Recognize **near-duplicate** notifications that exact-match dedup misses, using an LLM, at two trigger points: **creation-time** (before a new notification is persisted) and a **startup sweep** (a bounded pass to catch dups that slipped through).
- Treat "N agents reported the same thing" as a first-class **priority signal**: a `score` on each notification, incremented atomically on every fold, surfaced as a priority affordance in the Notifications UI.
- Allow the LLM to fold *new information* from a duplicate into the canonical via **minor, bounded edits**, with full **provenance** (we can always tell a notification was edited by a merge, and which duplicate caused it).
- Gate the entire behavior behind a **feature flag**, **off by default** (off-safe), gating both the creation-time and sweep paths. With the flag off, behavior is byte-for-byte today's exact-match dedup.
- Keep the comparison set **tractable** — bounded candidate sets at creation, bucketed comparisons at sweep, no O(n²) blow-up.
- Make the sweep **idempotent**: safe to run repeatedly, never thrashing or looping notifications.
- **Layer on, don't replace**, the existing exact dedup. The cheap deterministic `grouping_key` / `content_key` paths run first and unchanged; the LLM only adjudicates what exact matching leaves ambiguous.

## Non-goals

- **`WorkAttentionItem` (legacy operational alerts).** Those are engine-raised (worker failed, repo unresolved, tracker sync failed), not agent-authored, and already idempotent via `upsert_external_tracker_attention`. They are not what the user means by "notifications" here and are out of scope. (If we ever want dedup there, the `attention_merges` ledger shape generalizes — noted as future.)
- **Re-clustering / un-merging.** Once folded, a duplicate stays folded. We do not build an "actually these were different, split them" path in v1 (an operator can dismiss the canonical and the source agents will re-flag).
- **Cross-product dedup.** The comparison scope is a single product. A notification in product A never folds into one in product B.
- **Replacing exact dedup with the LLM.** `grouping_key` / `content_key` remain the first and cheapest line; the LLM is strictly additive.
- **Embeddings / a vector index.** v1 uses a cheap lexical prefilter + an LLM adjudication. A learned embedding index is a future optimization, not a v1 blocker.
- **Rewriting notification content wholesale.** Canonical-edit-on-merge is deliberately *minor and bounded* (see Chosen approach); it is not a summarization or merge-of-bodies feature.
- **A periodic background sweep on a timer.** The sweep runs on engine startup only (the project's stated "likely just on startup, since it should rarely be needed"); a recurring timer is explicitly deferred.

---

## Alternatives considered

### Alternative A — Pure deterministic dedup (extend `grouping_key` / embeddings, no LLM)

Make the dedup smarter without an LLM: normalize and fuzzy-hash the grouping key, or compute embeddings for each notification and fold when cosine similarity exceeds a threshold.

**Rejected for v1.** A better hash still only catches lexically-close keys — it cannot tell that two differently-phrased concerns are the same, which is exactly the failure mode (different agents, different wording, different anchors). Embeddings get closer but introduce a model + vector store + a similarity threshold to tune, and a threshold-only decision still can't produce the *bounded minor edit* the project requires ("fold in new info"). The semantic judgment ("are these the same concern, and what does the second add?") is precisely what an LLM is good at and a similarity score is not. Embeddings remain attractive as a future *prefilter* (cheaper candidate selection) feeding the same LLM adjudication — captured as a future task, not a v1 path.

### Alternative B — Always create, then sweep-only reconciliation (no creation-time check)

Let every agent create its notification freely; rely solely on a periodic/startup sweep to fold dups afterward.

**Rejected as the sole mechanism.** It guarantees a window where the user sees N duplicate cards before the sweep runs, and an on-startup-only sweep means that window can be hours. It also makes the score lag reality. Creation-time dedup keeps the Notifications window clean in the common case (an agent flags something that's already there) and makes the score accurate the moment the second agent reports. The sweep is kept as a **backstop** (Goal: "catch dups that slipped through"), not the primary path. The chosen design does both, with creation-time as primary.

### Alternative C — Spawn a worker to do the reconciliation

Spawn a normal Claude worker whose prompt is "look at the open notifications and merge the duplicates."

**Rejected.** This is the wrong tool for a tight, frequent, structured decision. A worker is heavyweight (a cube lease, a full agent session), slow, costly, and returns prose we'd have to parse. It also can't be invoked synchronously inside the notification-creation transaction. The decision here is a bounded prose-to-JSON transform — exactly what the existing engine-internal Anthropic substrate (`pane_summary.rs`) does for pane summaries — so a direct, structured-output API call is the right shape. (Same reasoning the [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) design used to reject an interactive worker for its Planner.)

### Alternative D (chosen) — Engine-internal LLM dedup decision, layered on exact dedup, at creation + startup sweep

Keep exact dedup as the first line. When (and only when) a notification would become a *new* group, run an engine-internal structured-output LLM call against a bounded set of existing open notifications; if it returns a canonical match, fold instead of create (`score += 1`, provenance row, bounded edit). A bounded, idempotent startup sweep applies the same decision to existing groups as a backstop. Everything behind an off-by-default flag. This is the rest of the document.

---

## Chosen approach

### Architecture overview

```
  CREATION-TIME PATH                                     STARTUP SWEEP PATH
  create_attention / reconcile_attentions                engine boot, flag on
        │                                                       │
        ▼                                                       ▼
  exact dedup first  ─── grouping_key / content_key       bucket open groups by
  (unchanged)             match? → join existing,          (product, kind, association)
        │                 no LLM, return                         │
        │ would create a NEW group                               ▼  per bucket, bounded
        ▼  (flag on?)                                       pick canonical (oldest/lowest A#)
  prefilter open groups (same product/kind/assoc,          compare each other vs canonical
  recency window) → top-K candidate set                          │
        │                                                        ▼
        ▼                                                  LLM dedup decision (batched)
  LLM dedup decision (candidate vs top-K)                        │
        │                                                        ▼
   duplicate? ── no ──► create new group (score=1)         fold losers: score += n,
        │ yes                                              attention_merges rows,
        ▼                                                  retire loser (merged_into +
   FOLD into canonical:                                    state=dismissed), bounded edits
     • score += 1  (atomic)                                      │
     • attention_merges provenance row                           ▼
     • bounded minor edit to canonical (optional)         idempotent: already-merged &
     • suppress candidate (do not persist new group)      already-compared pairs skipped
        │
        ▼
   return canonical group → AttentionCreated event → UI
```

The engine owns everything. The single LLM step is the dedup decision; it is a pure transform (no writes). Exact dedup runs first and unchanged. The flag gates both paths.

### 1. The unit of dedup is the `AttentionGroup` (the card)

A user perceives one Notifications card = one notification. That card is an `AttentionGroup`; its content lives in the `Attention` members. So:

- **Dedup, score, and provenance are group-level.** The candidate is a *would-be group* (with its initial members); the canonical is an existing group.
- **The LLM reasons over rendered content**, not raw keys: for each group we build a compact text rendering (kind, association, and each member's `prompt_text` / `proposed_name` + `proposed_description` / `rationale`). This is what an agent "flagged," in words.
- **Member-level exact dedup is unchanged.** When a candidate *does* share a grouping key with an existing group, `reconcile_attentions`'s `content_key` path still handles it with no LLM. The LLM only runs when exact matching would otherwise spawn a *new* group.

### 2. Data model — score + provenance

#### `score` on `attention_groups`

A single new column, added by an idempotent migration in the established style (`engine/core/src/work/migrations_b.rs`):

```sql
ALTER TABLE attention_groups ADD COLUMN score INTEGER NOT NULL DEFAULT 1;
```

- **Default `1`** — a freshly created notification has been "reported once." This makes the score a clean count of independent reports and means existing rows backfill to a sensible `1` with no data migration.
- **Atomic increment** — folding always runs inside the existing creation/sweep transaction:
  ```sql
  UPDATE attention_groups SET score = score + ?delta WHERE id = ?canonical_id;
  ```
  Single-statement, single-transaction; no read-modify-write race. (Creation folds `delta = 1`; the sweep may fold a cluster's loser count in one statement.)
- **Mapper + protocol.** `score: i64` is added to `boss_protocol::AttentionGroup` (a `bon::Builder` struct — additive optional fields need only `#[builder(default = 1)]`, no construction-site churn per the builder convention) and read in `map_attention_group` (`engine/core/src/work/mappers.rs`), which must explicitly map the new column (DB mappers stay struct-literal per repo convention).

#### `merged_into_group_id` on `attention_groups` (sweep retirement)

```sql
ALTER TABLE attention_groups ADD COLUMN merged_into_group_id TEXT;  -- nullable
```

The **creation-time** path *suppresses* the candidate before it is persisted — there is no loser row to retire. The **sweep** path, however, operates on already-persisted groups; a folded loser is retired by setting `merged_into_group_id = <canonical>` and `state = 'dismissed'` (with `dismissed_at`). This (a) removes it from the actionable list (`list_attention_groups` defaults to `open` + `partially_answered`), (b) preserves it for history/provenance instead of deleting, and (c) is the marker that makes the sweep idempotent (a row with `merged_into_group_id` set is never re-considered).

#### `attention_merges` provenance ledger

One row per fold — the durable record that a fold happened, why, and what changed:

```sql
CREATE TABLE IF NOT EXISTS attention_merges (
  id                  TEXT PRIMARY KEY,        -- merge_<...>
  canonical_group_id  TEXT NOT NULL REFERENCES attention_groups(id),
  product_id          TEXT NOT NULL,
  trigger             TEXT NOT NULL,           -- 'creation' | 'sweep'
  -- Creation-time: the candidate is never persisted, so we capture its identity inline.
  -- Sweep: the loser is a real row; its id is recorded and it is retired (merged_into_group_id).
  duplicate_group_id  TEXT,                    -- set for 'sweep'; NULL for 'creation'
  candidate_summary   TEXT NOT NULL,           -- the rendered candidate text (what was folded)
  candidate_source    TEXT,                    -- source_run_id / source_task_id / source_kind of the dup
  model               TEXT NOT NULL,           -- model slug used for the decision
  decision_rationale  TEXT,                    -- the LLM's short "why duplicate" note (verbatim)
  edits_applied       TEXT,                    -- JSON: per-field before/after, or NULL if none
  created_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS attention_merges_canonical_idx
  ON attention_merges(canonical_group_id, created_at);
-- Sweep idempotency: never fold the same (canonical, duplicate) pair twice.
CREATE UNIQUE INDEX IF NOT EXISTS attention_merges_pair_uq
  ON attention_merges(canonical_group_id, duplicate_group_id)
  WHERE duplicate_group_id IS NOT NULL;
```

`edits_applied` is what answers "was this notification edited by a merge, and by which duplicate?" — it stores the canonical's affected fields before and after, keyed to the `attention_merges.id`, so the UI can show "edited by merge" provenance and an operator can audit every change.

### 3. The dedup decision (the one LLM step)

A pure transform reusing the existing engine-internal Anthropic substrate (`pane_summary::claude_short_summary` pattern: shared reqwest client, `x-api-key` + `anthropic-version` headers, POST to `api.anthropic.com/v1/messages`; typed outcomes modeled on `live_status::SummarizerOutcome` — `Success` / `NoApiKey` / `ApiError` / `Transport`). It performs no writes.

#### Contract

```rust
// boss-protocol — shared so every caller and tests speak the same shape.

pub struct DedupInput {
    pub candidate: NotificationBrief,       // rendered candidate (kind, assoc, member text)
    pub existing: Vec<NotificationBrief>,   // the bounded comparison set (top-K)
}

pub struct NotificationBrief {
    pub group_id: String,                   // canonical id, or "candidate" sentinel
    pub kind: String,                       // "question" | "followup"
    pub association: String,                // project/task label, for scoping context
    pub rendered: String,                   // member text rendered to prose
}

pub struct DedupDecision {
    pub is_duplicate: bool,
    pub canonical_group_id: Option<String>, // which existing group, when is_duplicate
    pub confidence: Confidence,             // High | Medium | Low
    pub rationale: String,                  // short "why" — persisted to attention_merges
    pub proposed_edits: Vec<CanonicalEdit>, // bounded; may be empty
}

pub struct CanonicalEdit {
    pub member_ordinal: i64,                // which member of the canonical
    pub field: EditableField,               // RationaleAppend | DescriptionAppend
    pub new_text: String,                   // the appended/replacing text (bounded length)
}

pub enum EditableField { RationaleAppend, DescriptionAppend }
pub enum Confidence { High, Medium, Low }
```

- **Structured output is enforced**, not requested: a single forced tool call whose input schema is `DedupDecision` (the same forced-tool pattern the codebase will need; `pane_summary` currently uses plain text, so this is the one substrate extension). The engine deserializes straight into the Rust type; a deserialization failure is a decision failure (fail safe → treat as *not* a duplicate, create normally), never parse-and-hope.
- **`is_duplicate` requires a `canonical_group_id` that is in the input set.** A hallucinated id → treated as not-a-duplicate (fail safe, create normally). Validated in engine code, not trusted from the model.
- **Only the candidate folds into an existing canonical** — the decision never proposes creating, deleting, or merging two *existing* groups at creation time (that is the sweep's job, and even there only loser→canonical).

#### Model / effort tier

The creation-time decision is **frequent** (potentially every notification) and **bounded** (one candidate vs top-K short renderings) — a binary semantic-similarity judgment, not generation. So it defaults to a **fast, cheap tier (Haiku)** with a tight `max_tokens`, mirroring how `live_status` uses a cheap model for its one-liner. Quality is protected by (a) the cheap exact-dedup first line removing the easy cases, (b) the bounded prefilter giving the model only plausible candidates, and (c) confidence handling (below). The model is a single tunable constant. **Open question (R1):** whether Haiku is sufficient or the decision warrants Sonnet — start cheap, measure false-fold rate, upshift if needed. The sweep, being rare and batchable, may use a stronger tier without a frequency concern.

#### Confidence handling

- **High / Medium** → fold.
- **Low** → **do not fold** at creation time (create the candidate normally; a false fold is worse than a missed one — it hides a distinct concern). The sweep, with a human reviewing the Notifications window afterward, may fold Low only when flagged visibly. **Open question (R2):** exact threshold behavior.

### 4. Keeping the comparison set tractable

#### Creation-time (1 candidate vs top-K)

1. **Exact dedup first** (unchanged). The LLM only runs if a *new* group would be created — most repeat flags never reach it.
2. **Cheap prefilter** to a bounded **top-K** (e.g. K ≤ 8): restrict to open / partially-answered groups in the **same product**, prefer the **same `kind`** and **same association** (project/task), within a **recency window** (e.g. groups created/last-touched in the last N days), ranked by a lexical-overlap score (shared significant tokens between candidate rendering and each existing rendering). Only the top-K renderings go to the LLM.
3. **Skip entirely** if the prefilter set is empty (no LLM call) — the candidate is novel; create it.

This is **O(open-groups-in-product)** for the prefilter (a single indexed query + cheap scoring) and **exactly one** LLM call with a bounded input. No pairwise explosion.

#### Startup sweep (bounded, bucketed)

The naive "compare all pairs" is O(n²). Instead:

1. **Bucket** open groups by `(product_id, kind, association)`. Cross-bucket pairs are never compared (a question in project X is not a dup of a followup in task Y).
2. Within each bucket, **pick a deterministic canonical** (lowest `short_id`, i.e. oldest A-number; ties broken by `created_at`, then `id`).
3. **Compare each non-canonical member of the bucket against the canonical(s)** in one batched LLM call per bucket (candidate-vs-set, reusing the same contract). For buckets large enough to matter, cap the bucket size considered per sweep and `log()` any remainder rather than silently dropping it.
4. Fold losers into their canonical (cluster fold: `score += loser_count`, one `attention_merges` row per loser, retire each loser).

Buckets keyed on `(product, kind, association)` are small in practice (a handful of open cards per project), so per-sweep cost is bounded and the sweep is "rarely needed" as the project expects.

### 5. Canonical-edit-on-merge — bounded + recorded

The LLM **may** fold new information from the duplicate into the canonical, but tightly constrained:

- **Append-only to free-text fields only.** Editable fields are `Attention.rationale` and `Attention.proposed_description` (followup members) — the *explanatory* prose. **Never** editable: `question_type`, `choice_options`, `kind`, `prompt_text` of a question (changing the question itself would invalidate an in-progress human answer), `answer`, `answer_state`, `association`, or the group's structure/membership.
- **Length-bounded.** Each `CanonicalEdit.new_text` is capped (e.g. ≤ 200 chars), and the total edit per fold is capped. Over-budget edits are rejected (the fold still happens; the edit is dropped and `log()`-ed).
- **Only on still-open content.** Edits apply only to members whose `answer_state == "open"`. A member a human has already answered/skipped is frozen — a merge must never rewrite content under a human's feet.
- **Recorded verbatim.** Every applied edit is stored in `attention_merges.edits_applied` as `{member_ordinal, field, before, after}`. This is the provenance: the UI can render an "edited by merge" marker, and any change is fully auditable. An empty/no-edit fold (just `score += 1`) is the common, safe default.

### 6. Feature flag

Boss has no general feature-flag framework — config is environment-driven (`WorkConfig::load_from_env`, `engine/core/src/config.rs`). The flag follows that exact pattern:

- **Field:** `WorkConfig.notification_dedup_enabled: bool`.
- **Env var:** `BOSS_NOTIFICATION_DEDUP` (parsed like the existing `BOSS_*` vars).
- **Default: `false` (off-safe).** With the flag off: the creation-time LLM check is never reached (exact dedup runs exactly as today), and the startup sweep is not scheduled. The score column still exists and defaults to `1`, so the data model is forward-compatible whether or not the flag is ever turned on.
- **What each path checks:** the creation path consults the flag at the point it would otherwise create a new group (after exact dedup, before the prefilter); the sweep consults it at boot before scheduling. One field, two read sites — no scattered conditionals.
- **Degradation independent of the flag:** even with the flag *on*, a `NoApiKey` / `ApiError` / timeout outcome fails safe to "create normally" (creation) or "skip this bucket" (sweep) — the absence of an API key never blocks notification creation. (Mirrors how `live_status` degrades on `NoApiKey`.)

### 7. Startup sweep — trigger & idempotency

- **Trigger:** a one-shot background task spawned at engine boot (the `merge_poller`-style pattern — spawned by the coordinator, runs a single `run_one_pass`-equivalent), gated by `notification_dedup_enabled`. Not a recurring timer (deferred; see Non-goals).
- **Idempotency — the cardinal requirement (must not thrash):**
  - **Deterministic canonical selection** (lowest `short_id`) means repeated runs converge on the same canonical, never oscillating.
  - **Retired losers are inert:** a group with `merged_into_group_id` set (or any terminal state) is excluded from every future bucket, so it can never be re-folded or counted again.
  - **The `attention_merges` pair-unique index** (`(canonical_group_id, duplicate_group_id)`) makes a repeat fold of the same pair a no-op at the DB level — a hard backstop against double-counting the score.
  - **No re-splitting, ever** (Non-goal), so the sweep only ever *reduces* the open set; it has a fixed point and reaches it.
  - **Score is never recomputed from scratch** — it is only ever incremented at the moment of a (newly-recorded) fold, so a re-run that finds nothing new changes nothing.
- **Bounded work:** per-bucket caps + `log()` of any remainder (no silent truncation), one batched call per non-trivial bucket.

### 8. Surfacing the score as priority (UI)

- **Protocol/event:** `AttentionGroup.score` rides the existing `AttentionCreated` / group-list events; a fold publishes an update so the canonical card's score refreshes live.
- **macOS app (`AttentionsView.swift`, `ChatViewModel.swift`):**
  - A **score badge** on the group card when `score > 1` (e.g. a "×3" / "3 agents flagged this" chip), styled as a priority cue.
  - **Score-aware ordering** of open groups: the current open list (newest-first) becomes **score-desc, then created-at-desc**, so the most-corroborated concerns rise to the top. (A small, contained change to the `openGroups` computed property.)
  - **Merge provenance affordance:** where a member was edited by a merge, a subtle "edited by merge" marker; the card's detail can surface the `attention_merges` rationale ("folded 2 duplicate reports").
- No new window or RPC surface beyond the score field and a read of `attention_merges` for the provenance detail.

### 9. Edge cases

| Case | Handling |
|---|---|
| Flag off | Creation LLM never runs; sweep not scheduled; exact dedup as today. Score column present, defaults to `1`. |
| No API key / API error / timeout (flag on) | Fail safe: create normally (creation) / skip bucket (sweep). Never blocks notification creation. |
| LLM returns a `canonical_group_id` not in the input set | Treated as not-a-duplicate; create normally. Validated in engine code. |
| Low confidence | Creation: do not fold (create normally). Sweep: fold only if visibly flagged. |
| Candidate shares a grouping key with an existing group | Exact path (`reconcile_attentions` / `content_key`) handles it; LLM not invoked. |
| Canonical already answered/terminal | Not an edit target (edits only on `open` members); group-terminal canonicals are excluded from the comparison set, so a dup forms a fresh notification rather than folding into a closed one (consistent with existing generation semantics). |
| Sweep run twice | No-op for already-folded pairs (`attention_merges` pair-unique index); deterministic canonical; score unchanged. |
| Cluster of 3+ dups in the sweep | All non-canonical fold into the single deterministic canonical; `score += loser_count` in one statement; one provenance row per loser. |
| Edit exceeds length budget | Fold still happens (`score += 1`); the over-budget edit is dropped and `log()`-ed. |
| Empty prefilter set at creation | No LLM call; create the candidate (it's novel). |

---

## Risks / open questions

**R1 — Model tier for the creation-time decision.** Default proposed: Haiku (frequent, bounded, binary judgment). *Open:* is Haiku's precision adequate, or does the false-fold cost justify Sonnet? Start cheap; the model is a single tunable constant; measure false-fold rate before upshifting.

**R2 — False folds hide distinct concerns.** Folding two *different* concerns is worse than missing a dup — it silently suppresses a real notification. Mitigations: exact dedup first, bounded prefilter, fold only on High/Medium confidence at creation, full `attention_merges` provenance so a wrong fold is auditable and visible. *Open:* should creation-time folds be *staged* (score++ but the candidate also surfaced with a "possible duplicate" hint) rather than fully suppressed, for the first rollout? Current proposal: suppress on High/Medium, since the sweep + provenance give recourse.

**R3 — Score semantics.** Proposed: `score` = count of independent reports, default `1`. *Open:* should an explicitly-dismissed-then-re-flagged concern increment the *new* generation's score or carry the old one forward? Current proposal: a new generation starts fresh at `1` (consistent with the existing generation reset on terminal groups).

**R4 — Off-safe default vs. discoverability.** Flag defaults off, which means the feature does nothing until an operator opts in. *Open:* is off-by-default the right launch posture, or should it ship on for a canary product first? Proposed: off, flip on after observing sweep behavior on real data.

**R5 — Editing under a human's feet.** Bounded to append-only, open-members-only, length-capped, recorded. *Open:* is even append-to-rationale too much for v1 — should canonical-edit-on-merge be deferred entirely and v1 ship score-only folds? (See task breakdown: the edit task is separable and could be `future`.)

**R6 — Sweep cost on large products.** Bucketing keeps it bounded in practice, but a product with many open cards in one `(kind, association)` bucket could be expensive. Mitigation: per-bucket cap + `log()` remainder. *Open:* what cap, and should over-cap buckets defer to a follow-up sweep rather than being dropped?

**R7 — Structured-output substrate extension.** `pane_summary.rs` currently does plain-text completion; this needs a forced-tool-call / JSON-schema-constrained variant. Low risk (well-trodden API feature) but it is net-new substrate code. *Open:* build it as a small reusable `structured_call` helper alongside `claude_short_summary` so the [`auto-populate`](auto-populate-project-tasks-on-design-pr-merge.md) Planner (which needs the same) can share it.

**R8 — Interaction with `attentions`' generation model.** Folding must respect generations: a dup of an `actioned`/`dismissed` group should form a new notification, not fold into a closed one. Handled by excluding terminal groups from the comparison set; flagged here so a reviewer confirms it matches the intended [`attentions`](attentions.md) lifecycle.

---

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. Tasks at the same depth with no edge between them may run in parallel.

1. **Schema + score field + provenance ledger** (`boss-engine`). Idempotent migration adding `score INTEGER NOT NULL DEFAULT 1` and `merged_into_group_id TEXT` to `attention_groups`, creating the `attention_merges` table + its indexes (including the pair-unique sweep-idempotency index). Add `score` (and the atomic `UPDATE ... SET score = score + ?` increment helper) and `merged_into_group_id` to `map_attention_group` / list queries; add `score: i64` to `boss_protocol::AttentionGroup` with `#[builder(default = 1)]`; `WorkDb` accessors for `attention_merges` (insert/list). **Effort:** `medium`. **Depends on:** none.

2. **Feature-flag plumbing** (`boss-engine`). Add `notification_dedup_enabled: bool` to `WorkConfig` + `WorkConfigBuilder`, parse `BOSS_NOTIFICATION_DEDUP` in `load_from_env` (default `false`), and thread it to the two read sites (creation path, boot). No behavior change yet — just the gate, defaulting off. **Effort:** `small`. **Depends on:** none.

3. **Structured-output dedup-decision substrate + contract** (`boss-protocol`, `boss-engine`). Define `DedupInput` / `DedupDecision` / `NotificationBrief` / `CanonicalEdit` / `Confidence` in `boss-protocol`; add a reusable `structured_call` helper alongside `pane_summary::claude_short_summary` (forced tool call / JSON-schema-constrained output, typed outcomes modeled on `SummarizerOutcome`); implement `decide_dedup(DedupInput) -> Result<DedupDecision>` with the system prompt, model-tier constant (default Haiku), `max_tokens` bound, and engine-side validation (canonical id ∈ input set; else not-a-dup). No callers yet. **Effort:** `large`. **Depends on:** none (but the prefilter/rendering helpers in task 4 consume its types).

4. **Comparison-set prefilter + rendering helpers** (`boss-engine`). The `NotificationBrief` renderer (group → prose) and the creation-time prefilter (same product/kind/association, recency window, lexical-overlap top-K). Pure, unit-testable; shared by creation and sweep. **Effort:** `medium`. **Depends on:** 3 (for the `NotificationBrief` type).

5. **Dedup-at-creation path** (`boss-engine`). Hook into `create_attention` / `reconcile_attentions` at the "would create a new group" point: when flag on and exact dedup misses, run prefilter → `decide_dedup`; on a High/Medium duplicate, fold (atomic `score += 1`, `attention_merges` row with `trigger='creation'`, suppress the candidate group, return the canonical) inside the existing transaction; else create normally. Fail-safe on any LLM error. **Effort:** `large`. **Depends on:** 1, 2, 3, 4.

6. **Canonical-edit-on-merge (bounded + recorded)** (`boss-engine`). Apply `DedupDecision.proposed_edits` under the bounds (append-only to `rationale` / `proposed_description`, open-members-only, length caps), record before/after in `attention_merges.edits_applied`, drop+`log()` over-budget edits. Consumed by both the creation and sweep folds. *Separable — could ship as `future` (score-only folds in v1) per R5.* **Effort:** `medium`. **Depends on:** 1, 5.

7. **Startup sweep** (`boss-engine`). Boot-time one-shot background task (flag-gated), `merge_poller`-style: bucket open groups by `(product, kind, association)`, deterministic canonical (lowest `short_id`), batched per-bucket `decide_dedup`, cluster-fold losers (`score += n`, retire via `merged_into_group_id` + `state='dismissed'`, one `attention_merges` row each), per-bucket cap with `log()` remainder. Idempotent via the pair-unique index + retired-loser exclusion + deterministic canonical. **Effort:** `large`. **Depends on:** 1, 2, 3, 4 (6 if edits-on-sweep wanted).

8. **UI priority surfacing** (`app-macos`). `score` badge on cards (`score > 1`), score-desc-then-recency ordering of open groups, and a merge-provenance affordance ("edited by merge" marker + folded-count detail reading `attention_merges`). Thin client over the score field + a provenance read. **Effort:** `medium`. **Depends on:** 1 (score in protocol/events); benefits from 5/7 producing real scores but does not block on them.

9. **Tests: end-to-end dedup + idempotency** (`boss-engine`). Fixtures of near-duplicate notifications (different grouping keys, same concern); assert creation-time fold (score++, suppression, provenance), sweep clustering + double-run idempotency (no double count), flag-off no-op, fail-safe-on-no-api-key, and edit-bounds enforcement. **Effort:** `large`. **Depends on:** 5, 7 (6 if shipped).

**Parallelism / graph.** Depth 0 (no deps): **1**, **2**, **3** run in parallel. Depth 1: **4** (needs 3). Depth 2: **5** (needs 1,2,3,4) and **8** (needs 1) run in parallel. Depth 3: **6** (needs 1,5) and **7** (needs 1,2,3,4) run in parallel. Depth 4: **9** (needs 5,7[,6]).

**Deferred / not a v1 blocker:**
- **Canonical-edit-on-merge (task 6)** — `future` if R5 lands on score-only folds for v1; the fold path (task 5/7) works without it.
- **Embedding-based prefilter** — `future` optimization replacing/augmenting the lexical prefilter (Alternative A); feeds the same `decide_dedup`.
- **Recurring (timer-based) sweep** — `future`; v1 is startup-only per the project scope.
- **Dedup for `WorkAttentionItem` (operational alerts)** — `future`; out of scope for v1, but the `attention_merges` ledger shape generalizes.
- **Un-merge / re-split** — `future`; explicitly a Non-goal for v1.

---

## References

- [`attentions`](attentions.md) — the `AttentionGroup` / `Attention` model, `grouping_key` / `generation` group idempotency, `content_key` member dedup, lifecycle/generation semantics this design layers on.
- [`auto-populate-project-tasks-on-design-pr-merge`](auto-populate-project-tasks-on-design-pr-merge.md) — prior art for an engine-internal structured-output LLM step + deterministic apply; shares the structured-call substrate need (R7).
- Code anchors: `AttentionGroup` / `Attention` (`tools/boss/protocol/src/types.rs`); `create_attention` / `reconcile_attentions` / `resolve_or_create_group` / `list_attention_groups` (`tools/boss/engine/core/src/work/attentions.rs`); `map_attention_group` (`tools/boss/engine/core/src/work/mappers.rs`); schema + migrations (`tools/boss/engine/core/src/work/schema_init.rs`, `tools/boss/engine/core/src/work/migrations_b.rs`); Anthropic substrate (`tools/boss/engine/core/src/pane_summary.rs`, `tools/boss/engine/core/src/live_status.rs`); config/flag pattern (`tools/boss/engine/core/src/config.rs`); background-sweep pattern (`tools/boss/engine/core/src/merge_poller.rs`); UI (`tools/boss/app-macos/Sources/AttentionsView.swift`, `tools/boss/app-macos/Sources/ChatViewModel.swift`).

---

*Parent project: `Notification dedup + scoring`. Design-first; this doc proposes the implementation task graph above for downstream auto-population.*
