<!-- Ground-truth fixture for planner e2e tests (design task 11).
     Verbatim excerpt of the "Proposed implementation task breakdown" section of
     tools/boss/docs/designs/notification-dedup-scoring.md.
     Stands in as a representative recently-populated project (P757/P754 are
     Boss-DB project ids not present in the repo).
     Do not edit by hand except to re-sync with the source doc. -->

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. Tasks at the same depth with no edge between them may run in parallel.

1. **Schema + score field + provenance ledger** (`boss-engine`). Idempotent migration adding `score INTEGER NOT NULL DEFAULT 1`, `merged_into_attention_id TEXT`, and `linked_work_item_id TEXT` to `attentions`; adding `answer_state = 'merged'` as a recognized terminal value; creating the `attention_merges` table + its indexes (item-level ids including the pair-unique sweep-idempotency index, plus the `canonical_work_item_id` index for taxonomy dup provenance queries). Add `score`, `merged_into_attention_id`, `linked_work_item_id` to `map_attention` / list queries; add `score: i64` and `linked_work_item_id: Option<String>` to `boss_protocol::Attention` (with `#[builder(default = 1)]` and `#[builder(default)]` respectively); `WorkDb` accessors for `attention_merges` (insert/list/count-by-work-item); empty-card-cleanup helper (count open members after fold). **Effort:** `medium`. **Depends on:** none.

2. **Feature-flag plumbing** (`boss-feature-flags`, `boss-engine`). Append all three `FeatureFlagSpec` entries (`notification_dedup`, `notification_dedup_taxonomy`, `notification_dedup_sensibility`, all `default_enabled: false`, `category: "notifications"`) to `REGISTRY` in `tools/boss/engine/feature-flags/src/lib.rs`. Add the two `is_enabled("notification_dedup")` checks (creation path in `attentions.rs`, boot-time sweep scheduler in `app.rs`) and the two sub-flag checks (taxonomy prefilter, sensibility flag in `DedupInput`). No behavior change yet — just the gates, all defaulting off, with live debug-pane toggles. **Effort:** `trivial`. **Depends on:** none.

3. **Structured-output dedup-decision substrate + contract** (`boss-protocol`, `boss-engine`). Define `DedupInput` / `DedupDecision` / `DedupVerdict` / `AttentionBrief` / `WorkItemBrief` / `CanonicalEdit` / `Confidence` in `boss-protocol`; add a reusable `structured_call` helper alongside `pane_summary::claude_short_summary` (forced tool call / JSON-schema-constrained output, typed outcomes modeled on `SummarizerOutcome`); implement `decide_dedup(DedupInput) -> Result<DedupDecision>` with the system prompt (covering attention dup, work-item dup, and sensibility judgments in a single call), model-tier constant (default Haiku), `max_tokens` bound, and engine-side validation (canonical attention id ∈ `existing_attentions`; work item id ∈ `existing_work_items`; sensibility reason non-empty and entity-specific; else `Keep`). No callers yet. **Effort:** `large`. **Depends on:** none (but the prefilter/rendering helpers in task 4 consume its types).

4. **Comparison-set prefilter + rendering helpers** (`boss-engine`). The `AttentionBrief` renderer (item → prose, including association and parent group as context) and the creation-time prefilter (same product, recency window, widened top-K with same-association as a mild tiebreaker only — cross-task, cross-card items must enter the top-K). Pure, unit-testable; shared by creation and sweep. **Effort:** `medium`. **Depends on:** 3 (for the `AttentionBrief` type).

4a. **Taxonomy prefilter + `WorkItemBrief` rendering** (`boss-engine`). Query open work items (tasks, chores, revisions, projects) in non-terminal states for the same product; render each as a `WorkItemBrief`; populate `DedupInput.existing_work_items` up to the per-kind cap; `log()` any overflow. Gated by `notification_dedup_taxonomy`. Pure, unit-testable; shared by creation and sweep. **Effort:** `small`. **Depends on:** 3 (for `WorkItemBrief`), 2 (for the sub-flag check).

5. **Dedup-at-creation path** (`boss-engine`). Hook into `create_attention` / `reconcile_attentions` at the "would persist a new `Attention` item" point: when `notification_dedup` on and exact dedup misses, run prefilter (task 4) + taxonomy prefilter if `notification_dedup_taxonomy` on (task 4a) → `decide_dedup`; handle each verdict:
   - `AttentionDup` High/Medium → fold (atomic `score += 1` on canonical item, `attention_merges` row `trigger='creation'`, suppress candidate, return canonical);
   - `WorkItemDup` High → suppress-with-pointer (`attention_merges` row with `canonical_work_item_id`, no Attention row created);
   - `WorkItemDup` Medium → create Attention with `linked_work_item_id` set;
   - `Sensibility` High (if sensibility flag on) → suppress with `attention_merges` row `trigger='sensibility'`;
   - all else → create normally.
     Fail-safe on any LLM error. **Effort:** `large`. **Depends on:** 1, 2, 3, 4, 4a.

6. **Canonical-edit-on-merge (bounded + recorded)** (`boss-engine`). Apply `DedupDecision.proposed_edits` under the bounds (append-only to `rationale` / `proposed_description`, open-items-only, length caps), record before/after in `attention_merges.edits_applied`, drop+`log()` over-budget edits. Consumed by both the creation and sweep folds. _Separable — could ship as `future` (score-only folds in v1) per R5._ **Effort:** `medium`. **Depends on:** 1, 5.

7. **Startup sweep** (`boss-engine`). Boot-time one-shot background task (flag-gated), `merge_poller`-style: bucket open `Attention` items by `(product[, kind])` — **not** by association/task or card, so cross-task dups are visible within each bucket — apply recency window, deterministic canonical (lowest parent `short_id`, then earliest item `created_at`), batched per-bucket `decide_dedup` (with `existing_work_items` populated if taxonomy flag on; `sensibility_check: false` in the sweep per R14), cluster-fold losers (`score += n` on canonical item, retire via `merged_into_attention_id` + `answer_state='merged'`, one `attention_merges` row each, empty-card cleanup for each affected group), per-bucket cap with `log()` remainder. `WorkItemDup` High → retire loser with `canonical_work_item_id` in the merge row. Idempotent via the pair-unique index + retired-item exclusion + deterministic canonical. **Effort:** `large`. **Depends on:** 1, 2, 3, 4, 4a (6 if edits-on-sweep wanted).

8. **UI priority surfacing** (`app-macos`). `score` badge on items and their parent card (`score > 1`), score-desc-then-recency ordering of open groups (using max item score), and a merge-provenance affordance ("edited by merge" marker + folded-count detail reading `attention_merges`). Thin client over the score field + a provenance read. **Effort:** `medium`. **Depends on:** 1 (score in protocol/events); benefits from 5/7 producing real scores but does not block on them.

9. **Tests: end-to-end dedup + idempotency** (`boss-engine`). Fixtures of near-duplicate `Attention` items (different grouping keys, different cards, same concern); assert creation-time fold (score++, suppression, provenance), partial-card folding (other items in same card untouched), sweep clustering + double-run idempotency (no double count), empty-card cleanup (card retired when last open item folded), flag-off no-op, fail-safe-on-no-api-key, edit-bounds enforcement. Also: taxonomy-aware fixtures (attention dup of existing task → suppress-with-pointer at High, linked at Medium), sensibility fixtures (stale attention suppressed with reason, vague reason rejected → Keep), `WorkItemDup` with hallucinated id → Keep, sensibility at Medium → Keep. **Effort:** `large`. **Depends on:** 5, 7 (6 if shipped).

**Parallelism / graph.** Depth 0 (no deps): **1**, **2**, **3** run in parallel. Depth 1: **4** and **4a** (both need 3; 4a also needs 2) run in parallel. Depth 2: **5** (needs 1,2,3,4,4a) and **8** (needs 1) run in parallel. Depth 3: **6** (needs 1,5) and **7** (needs 1,2,3,4,4a) run in parallel. Depth 4: **9** (needs 5,7[,6]).

**Deferred / not a v1 blocker:**

- **Canonical-edit-on-merge (task 6)** — `future` if R5 lands on score-only folds for v1; the fold path (task 5/7) works without it.
- **Embedding-based prefilter** — `future` optimization replacing/augmenting the lexical prefilter (Alternative A); feeds the same `decide_dedup`.
- **Recurring (timer-based) sweep** — `future`; v1 is startup-only per the project scope.
- **Dedup for `WorkAttentionItem` (operational alerts)** — `future`; out of scope for v1, but the `attention_merges` ledger shape generalizes.
- **Un-merge / re-split** — `future`; explicitly a Non-goal for v1.

---
