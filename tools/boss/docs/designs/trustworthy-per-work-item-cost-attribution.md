# Boss: Trustworthy per-work-item cost attribution

- **Date:** 2026-07-30
- **Status:** Design — awaiting review. No implementation in this change.
- **Product:** Boss
- **Deliverable:** this document. The task breakdown at the end is the handoff to scheduling.
- **Evidence base:** `spinyfin/mono` @ `90e6c84b` (project brief, with live DB measurements taken 2026-07-29 20:15 CDT), re-verified against the working tree at `dea8ea14` on 2026-07-30. Every code claim below was re-read; DB-population claims are carried from the brief and are labelled as such.
- **Related:** [`codex-as-a-first-class-agent-driver.md`](codex-as-a-first-class-agent-driver.md), [`grok-as-a-first-class-interactive-agent-driver.md`](grok-as-a-first-class-interactive-agent-driver.md), [`agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md), [`engine-counter-metrics-framework.md`](engine-counter-metrics-framework.md)

## Verdict

Boss captures nine columns of token telemetry per run and cannot turn any of it into a cost figure a person should believe. The blocker is not arithmetic — it is that the `(driver, model, disjoint-token-class, count)` tuple pricing requires **does not exist in the database today**, for four independent reasons, in addition to there being no rate table and no read surface.

The design's core move is to stop treating cost as a nine-column widening of `work_runs` and start treating it as **two normalised facts recorded at capture time** — the driver that produced the run, and a per-model breakdown of _disjoint_ token classes — with pricing, roll-up, and presentation layered strictly on top. Everything downstream of that is mechanical. Everything upstream of it is currently wrong in a way that no downstream cleverness can repair.

The secondary move is to make **absence loud**. Every figure the system shows carries a coverage triple, and a row that cannot be priced shows why, never a zero.

## Goals

- A per-work-item spend figure the operator can believe, **or an honest refusal** where the data does not support one. A silently wrong figure is worse than no figure; this is the constraint the whole design is shaped around.
- Close the four capture defects so that runs recorded _from the fix forward_ carry the tuple pricing needs.
- Give model attribution that **survives a run** — recorded at capture time from the transcript, not re-derived later from mutable rows.
- Decide and defend the roll-up semantics (revision children, `pr_review` executions, board-level summation) rather than leaving them to whoever writes the first `SUM()`.
- Expose the result on a read surface, so that answering "what did this cost" stops requiring hand-written SQL against `state.db`.
- Make coverage a first-class part of every answer, so a row with 287 executions and 3 tokened runs cannot present itself as authoritative.

## Non-goals

- **Backfilling the ~4017 pre-instrumentation `in_review`/`done` rows. This is permanently impossible and must not be re-opened.** Boss stores only `work_runs.transcript_path`, a pointer into the provider's own directory. Claude and Codex both reclaim those files under their own retention sweeps, and Boss's `codex_home_retention_sweep` participates in that reclamation for Codex. The bytes are gone. No amount of engineering recovers a token count from a deleted JSONL file. The historical board is permanently uncosted; the design's answer is to _say so on those rows_, not to invent a number for them.
- **A Boss-internal mirror of transcript content to work around provider retention.** Copying provider transcripts into Boss-owned storage would make the retention problem go away and is explicitly rejected: it duplicates a large, sensitive, provider-owned artifact for a derived scalar, and it puts Boss in the business of storing conversation content it does not otherwise store.
- **Populating rate values in this document.** Model rates change and must come from the provider's published pricing at implementation time. This design specifies the _shape_ of the rate table, its provenance and effective-dating semantics, and its failure policy. It deliberately contains no numbers. Any figure recalled from a model's training data is not authoritative and must not enter the table.
- **Budget enforcement, spend caps, or dispatch gating on cost.** This project produces a trustworthy read. Acting on it — refusing to dispatch an expensive row, alerting on a runaway worker — is downstream work that this design makes possible but does not attempt.
- **A general time-series or cost-over-time analytics surface.** The unit of answer here is "what has this work item cost", not "plot org spend by week". Per-run rows retain their timestamps, so a later analytics layer is not foreclosed; it is just not built here.
- **Pricing non-token spend.** Web search calls, code execution, and similar metered provider surfaces are not captured today and are out of scope. Where a provider bundles them into a reported total, the design's oracle check (below) will surface the discrepancy rather than hide it.

## Background: what exists, and the four ways it is wrong

Instrumentation landed 2026-07-26 (commit `f01a5b3a6`). Nine columns hang off `work_runs` (`model`, `input_tokens`, `output_tokens`, `cache_creation_tokens`, `cache_read_tokens`, `cache_creation_5m_tokens`, `cache_creation_1h_tokens`, `rounds`, `agent_active_ms`), added by `migrate_work_runs_cost_columns` (`work/migrations_a.rs:445`). Capture lives in `engine/core/src/run_cost.rs`, driven from a single production call site in `app/worker_events.rs`, persisted through `work/executions_runs.rs::set_run_cost_snapshot`.

The existing capture machinery is in better shape than the raw defect list suggests, and the design should preserve rather than replace it. It already dedupes Claude's multi-record responses by `message.id`; it already treats Codex's cumulative `total_token_usage` as a replacement rather than an increment; it already refuses to invent a TTL split it did not observe (`cache_creation_ttl_split_known`); it already persists before any completion gate, so an orphaned run keeps the spend observed up to its last hook; and its writes are assignments, not increments, so retried hooks and post-restart rebuilds are idempotent. The incremental-tail design is sound. **What is wrong is the vocabulary it accumulates into and the provenance it fails to record.**

### Defect 1 — Grok records nothing

`CostAccumulator::ingest` (`run_cost.rs:69`) matches on a top-level `type` field. Grok's advertised transcript is `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl`, an ACP stream whose records are `{"timestamp", "method", "params"}` — no `type` at any level. Every record falls through to `_ => {}`. Zero grok slugs exist in `work_runs`.

Re-verification found the situation **better than the brief describes**, and this materially shrinks the fix. The usage is not merely present in a file Boss already tails; it is in a record the Grok driver **already parses and then discards**. `grok/transcript.rs:138` maps `sessionUpdate == "turn_completed"` into `AcpEnvelope::TurnCompleted`, keeping only `stop_reason`. The sibling `usage` object it drops is (verbatim, from `docs/investigations/ghostty-grok-pane-viability-artifacts/.../session_telemetry_excerpt.md`):

```json
{
  "inputTokens": 13599,
  "outputTokens": 33,
  "totalTokens": 13632,
  "cachedReadTokens": 13440,
  "reasoningTokens": 24,
  "modelCalls": 1,
  "apiDurationMs": 1348,
  "costUsdTicks": 45480000,
  "modelUsage": {
    "grok-4.5-build": {
      "inputTokens": 13599,
      "outputTokens": 33,
      "totalTokens": 13632,
      "cachedReadTokens": 13440,
      "reasoningTokens": 24,
      "modelCalls": 1,
      "apiDurationMs": 1348,
      "costUsdTicks": 45480000
    }
  },
  "numTurns": 1
}
```

Three things in that record matter beyond closing the defect:

- `inputTokens (13599) > cachedReadTokens (13440)` and `totalTokens = inputTokens + outputTokens`. **Grok's input is gross, like Codex's — not net, like Claude's.** Gross-versus-net is therefore a per-driver axis with two of three drivers on the gross side, which is the argument for fixing it as a vocabulary problem rather than as a Codex special case.
- `modelUsage` is a **native per-model breakdown**, keyed by model id. Grok solves the multi-model-run problem for free; the design's data model should be able to accept it rather than flattening it away.
- `costUsdTicks` is a **provider-computed cost**. Boss must not use it as its figure — it is not available for the other two drivers and would make cross-driver figures incomparable — but it is an independent oracle against which Boss's own computed Grok cost can be validated. That is a genuinely rare thing to have and the design uses it as an accuracy check on the rate table.

### Defect 2 — gross and net input share one column

A live Codex rollout in-repo (`docs/investigations/codex-exit-code-surfacing-artifacts/probes/p1_short_nonzero/`):

```json
{
  "input_tokens": 13982,
  "cached_input_tokens": 11008,
  "cache_write_input_tokens": 0,
  "output_tokens": 134,
  "reasoning_output_tokens": 0,
  "total_tokens": 14116
}
```

`13982 + 134 = 14116`, so `input_tokens` already contains the cached portion. Claude's `input_tokens` is the disjoint fresh-input count. Both are written to `work_runs.input_tokens`. Any formula of the shape `input × fresh_rate + cache_read × cached_rate` is correct for Claude and **double-charges the cached input of every Codex and every future Grok row** — measured at 92.8% of Codex input in the brief's live sample.

This cannot be repaired downstream, because the column does not record which convention produced it, and `work_runs` has no `driver`. A resolver does exist — `driver_transcript::resolve_execution_driver_slug` — and it is pool-dispatch-aware, which matters (`pr_review` and `automation_triage` always run on the review pool's fixed driver regardless of the reviewed row's own). But it derives its answer from `tasks.driver` → `products.default_driver` at _call_ time. Those rows are mutable and the engine default can change between releases. **Re-deriving the driver later is not the same fact as the driver that ran.** The design records it.

Codex coverage is also only 25.9% of post-instrumentation runs versus Claude's 99.0%. The leading suspect is the containment refusal at `app/worker_events.rs:379` — when `transcript_containment_root()` errs, `containment_root` becomes `None` and the entire `if let Some(containment_root)` capture block is skipped. Refusing the read is the _correct_ security posture (degrading a contained driver to an unrestricted tail would be a regression, and must not be the fix), but the skip is currently **silent and indistinguishable in the database from "this run used no tokens"**. That indistinguishability is itself a defect the design closes, independent of whatever the root cause turns out to be.

### Defect 3 — subagent tokens are excluded

Claude Code writes subagent transcripts to `~/.claude/projects/<slug>/<parent-session-uuid>/subagents/agent-<id>.jsonl`. Every record carries `isSidechain: true` and a full `usage` object, with **zero `message.id` overlap** with the parent transcript. Boss never tails them: no `SubagentStop` hook is wired (`CLAUDE_HOOK_EVENTS`, `driver/src/claude.rs:161`, lists seven events and not that one), so no hook ever advertises those paths, and `RunCostTail.tails` only gains an entry from a _differing advertised path_.

The brief measured **25.4% of fresh input and 12.4% of cache-creation uncounted** over the 18 token-bearing runs whose subagent files survive — a lower bound, and large enough that it alone disqualifies the current figure.

Two properties make the fix safe. The accumulator dedupes by `message.id`, and the ID spaces are disjoint, so folding subagent records in is purely additive with no double-count risk. And the exposure is Claude-specific by construction: the Grok driver spawns with `--no-subagents` (`driver/src/grok.rs:178`), whose in-code comment notes that Claude declares no equivalent flag precisely because its subagents emit.

Backfill here is _mostly_ impossible — only 18 of 331 runs still have their files — so this is a forward-capture fix. The design does not propose a partial backfill of the 18.

### Defect 4 — `<synthetic>` clobbers the real model

`CostAccumulator.model` is last-writer-wins (`run_cost.rs:106`) while token counts sum across every record. Claude Code appends a trailing synthetic assistant record whose model reads `<synthetic>`; a verified transcript with 379 `claude-opus-5` records and one `<synthetic>` — last — produced a DB row labelled `<synthetic>`. 24 runs are in this state, one carrying 64.7M cache-read tokens. Grepping `tools/boss` confirms no Boss code emits that string: it is the provider's own label, arriving through the front door.

Last-writer-wins is also wrong for a legitimate reason, not just a synthetic-record accident: a single run can genuinely span models (subagent transcripts show `claude-sonnet-5` and `claude-opus-5` mixed), and only one label survives. There is no fallback source — `tasks.model_override` is NULL/empty on all non-deleted tasks and `products.default_model` is empty on all products.

Observed `model` values across all 12,765 runs: `(NULL)` 12,418 · `claude-opus-5` 159 · `claude-sonnet-5` 142 · `<synthetic>` 24 · `gpt-5.6-terra` 14 · `opus` 1. The lone `opus` is a slug _alias_ — `claude_model_belongs_to_driver` (`driver/src/claude.rs:128`) accepts bare `"opus"`, and drivers pass such aliases through to their CLI — reaching a Codex `turn_context.payload.model`. A rate table keyed on raw observed strings would need entries for both `opus` and `claude-opus-5`, and would silently fail to price the next alias anyone adds. Canonicalisation is required, and it belongs to the driver, which is the only component that knows its own model namespace.

### Not captured: reasoning tokens

Codex emits `reasoning_output_tokens`; Grok emits `reasoningTokens`. Neither is read. For Claude, thinking is billed inside `output_tokens`, so nothing is lost. For the other two, whether the field is additive or already inside `output` is **not answerable from the samples in this repo**: the Codex sample above has `reasoning_output_tokens: 0`, and the Grok sample has `reasoningTokens: 24` against `outputTokens: 33` — consistent with containment but far from proof. Assuming it is free is exactly the kind of silent error this project exists to prevent, so this is sequenced as an explicit investigation before any pricing depends on it.

### There is no rate table and no read surface

`git grep -iE "per_million|price_per|rate_table|cost_per_|usd_per|pricing_table"` returns nothing across `tools/boss`. `bossctl` has no `cost`/`usage`/`tokens`/`spend` verb — verified against the command enums in `cli/src/commands.rs`. `boss task show --json` does not surface `work_runs` at all, so the nine columns are invisible on the wire. There is no macOS app surface and no sidecar file. Reading this today means hand-written SQL against `state.db`.

One durability point in Boss's favour: the execution retention prune (14 days, `work/execution_retention.rs:76`) excludes `completed` status by design, so captured spend on completed runs persists indefinitely. Whatever is captured correctly from the fix forward stays captured.

## Alternatives considered

### A. Price at capture time — write a USD figure into `work_runs`

Compute cost in `run_cost.rs` as records are ingested and persist a `cost_usd` scalar alongside the token columns.

**Rejected.** It fossilises the answer. Rates change, and a stored figure computed under last month's rates cannot be re-derived or corrected. More damagingly, it fossilises _bugs_: every one of the four capture defects would have been baked into an immutable number that looks authoritative and carries no evidence of how it was produced. Re-pricing after a fix would be impossible for exactly the same reason backfill is impossible. Tokens are the durable fact; dollars are a view over tokens and a dated rate, and must be computed on read.

### B. Use the provider's own reported cost

Grok reports `costUsdTicks` per turn. Consume it directly and skip the rate table.

**Rejected as the primary mechanism.** Only Grok reports it — Codex's rollout and Claude's transcript carry no cost field — so a board would mix provider-authoritative Grok figures with Boss-computed Claude and Codex figures, which are not comparable and whose disagreements would be invisible. It also imports the provider's bundling decisions (what counts as billable, how discounts apply) inconsistently across drivers, and provides no way to answer counterfactuals like "what would this row have cost on the cheaper model".

**Kept as an oracle.** Boss's computed Grok cost can be diffed against `costUsdTicks` on the same run. A systematic divergence is direct evidence that the rate table or the token vocabulary is wrong, and it is the only end-to-end accuracy check available anywhere in this system. This is worth real effort, and appears in the breakdown as a validation task.

### C. Per-driver correction factor for the gross/net collision

Keep the single `input_tokens` column and apply a per-driver multiplier at pricing time to undo Codex's double-count.

**Rejected, and forbidden by the project brief.** A factor derived from an aggregate ratio is not a correction, it is a fudge that happens to be near-right on average and arbitrarily wrong per row — cache hit rates vary enormously between a fresh run and a resumed one. It also encodes a per-driver quirk in the _pricing_ layer, which is precisely the wrong place: pricing should see one vocabulary and know nothing about who produced it. The gross/net difference is a source-format difference and belongs to the component that already owns source-format differences — the driver.

### D. Wall-clock or `agent_active_ms` proxy

Price runs by active agent time against a per-model hourly rate.

**Rejected.** It is not a cost model; it correlates with cost only through the model's throughput, which varies with tool use, cache state, and thinking. Boss already captures the real inputs, and substituting a proxy for data we have would be a strict downgrade. `agent_active_ms` remains useful as a _sanity_ signal — a run with tokens and no active time, or vice versa, is a capture anomaly worth surfacing — but never as a price.

### E. Do nothing; document the columns and let operators write SQL

**Rejected.** The four capture defects mean hand-written SQL over these columns produces confidently wrong answers today. Publishing the schema without fixing the vocabulary would make wrong figures _easier_ to produce, which is the opposite of the goal.

## Chosen approach

Five layers, strictly ordered. Each is independently useful; each depends only on the ones above it.

```
  5. Read surface        bossctl cost verbs · task show --json · (app, deferred)
  4. Roll-up             attribution rule · spend classes · coverage triple
  3. Pricing             dated rate table · unpriceable classification
  2. Attribution         per-(model) usage breakdown · canonical model ids
  1. Capture             driver-owned extraction into one disjoint vocabulary
  0. Provenance          work_runs.driver · work_runs.cost_capture_status
```

### Layer 0 — Record provenance at capture time

Two columns on `work_runs`:

- **`driver TEXT`** — the driver slug that actually produced this run, written when the first cost snapshot is persisted. Its _value_ comes from the existing `driver_transcript::resolve_execution_driver_slug`, which already applies the correct pool-dispatch-aware precedence; this is a reuse, not a new resolution path. What changes is that the answer is **snapshotted** rather than re-derived, so a later edit to `tasks.driver` or a change to the engine default cannot retroactively alter how a historical run is priced.
- **`cost_capture_status TEXT`** — one of `captured`, `skipped_containment_unresolved`, `skipped_no_transcript`, `partial_subagents_unavailable`, `no_usage_observed`. This is what makes the difference between "this run genuinely used no tokens" and "Boss declined to read this run" visible in the data. It is the mechanism by which the coverage story at layer 4 can be honest, and it is what turns the Codex coverage question from an unmeasurable suspicion into a query.

Both are set on the same idempotent-assignment path as the existing snapshot, so retries and post-restart rebuilds behave identically to today.

### Layer 1 — One disjoint token vocabulary, normalised by the driver

Define a canonical usage record whose fields are **mutually disjoint by construction**, so that a price is always `Σ (class_count × class_rate)` with no inclusion-exclusion anywhere:

| Class                         | Meaning                                                                                                                                                                                                                  |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `fresh_input`                 | Input tokens billed at the full uncached rate. Contains no cached-read and no cache-write tokens.                                                                                                                        |
| `cache_read`                  | Input tokens served from cache.                                                                                                                                                                                          |
| `cache_write_5m`              | Cache-creation tokens at the 5-minute TTL.                                                                                                                                                                               |
| `cache_write_1h`              | Cache-creation tokens at the 1-hour TTL.                                                                                                                                                                                 |
| `cache_write_unsplit`         | Cache-creation tokens whose TTL the provider did not report. Priced only if the rate table declares an unsplit rate for that model; otherwise the run is unpriceable. Never silently assigned to a TTL.                  |
| `output`                      | Output tokens, inclusive of any reasoning tokens the provider bills inside output.                                                                                                                                       |
| `reasoning_billed_separately` | Reasoning tokens **only** where the provider bills them as an additive class outside `output`. Zero for Claude by definition. Populated for Codex and Grok only if the investigation task establishes they are additive. |

Extraction moves behind a new capability on `AgentDriver`, alongside the transcript-normalisation capability the trait already carries (`normalize_transcript_entry`, `transcript_session`). Each driver maps its own dialect into the vocabulary above:

- **Claude** — already net. `fresh_input := input_tokens`, `cache_read := cache_read_input_tokens`, TTL split from `usage.cache_creation`. This is a behaviour-preserving move of today's logic, which is what makes it a safe first extractor.
- **Codex** — gross. `fresh_input := input_tokens − cached_input_tokens − cache_write_input_tokens`. **The exact containment relation must be confirmed against real rollouts before this ships** — the in-repo sample has `cache_write_input_tokens: 0`, which does not distinguish "inside input" from "alongside input". A negative result from that subtraction is a hard error, not a clamp to zero: it means the assumed relation is wrong and the run must be marked unpriceable rather than mispriced.
- **Grok** — gross. `fresh_input := inputTokens − cachedReadTokens`, from the `turn_completed` record's `usage`, and preferentially from its per-model `modelUsage` map so that layer 2 gets a real breakdown rather than a flattened total.

`run_cost.rs` keeps its incremental tail, its `message.id` dedup, its cumulative-versus-incremental handling, and its idempotent persistence. It stops knowing what an assistant record or a rollout looks like.

Subagent enumeration (defect 3) attaches here: when the driver declares that its transcripts have sidechains, the tail additionally enumerates the sibling `subagents/` directory for the advertised parent transcript and folds those files into the same accumulator. This does not require a `SubagentStop` hook — it derives the paths from the parent path Boss is already told about, which avoids adding a hook to every worker's settings. Directory enumeration failures degrade to `partial_subagents_unavailable`, not to silence.

### Layer 2 — Model attribution that survives the run

Usage is attributed to **the model named on the record that carried it**, not to a run-level last-writer-wins field. That requires a child table:

```sql
CREATE TABLE work_run_model_usage (
    run_id                       TEXT NOT NULL REFERENCES work_runs(id) ON DELETE CASCADE,
    driver                       TEXT NOT NULL,
    model_raw                    TEXT NOT NULL,  -- verbatim provider string
    model_canonical              TEXT,           -- NULL when unrecognised
    fresh_input_tokens           INTEGER NOT NULL,
    cache_read_tokens            INTEGER NOT NULL,
    cache_write_5m_tokens        INTEGER NOT NULL,
    cache_write_1h_tokens        INTEGER NOT NULL,
    cache_write_unsplit_tokens   INTEGER NOT NULL,
    output_tokens                INTEGER NOT NULL,
    reasoning_separate_tokens    INTEGER NOT NULL,
    PRIMARY KEY (run_id, driver, model_raw)
);
```

This is what makes `<synthetic>` a non-problem rather than a patch target. It is no longer a label competing to overwrite a real one — it becomes its own row, holding whatever usage its own records carried (in the observed case, effectively none), while `claude-opus-5` keeps the 379 records' worth that actually belongs to it. A genuinely mixed opus/sonnet run gets two rows and prices correctly. Grok's `modelUsage` map populates several rows natively.

`model_canonical` comes from a **driver-owned canonicalisation** — the driver already owns its model namespace via `ModelMenu` and `model_belongs_to_driver`, and is the only place that knows `opus` and `claude-opus-5` are the same billable thing. Retaining `model_raw` verbatim alongside it is deliberate: when canonicalisation fails, the operator needs to see what the provider actually said.

The existing `work_runs.model` column is retained as a **denormalised display hint** — the model accounting for the most output tokens on that run — so existing readers keep working. Pricing never reads it.

### Layer 3 — A dated rate table with a loud failure policy

A rate table keyed on `(driver, model_canonical, effective_from)`, with a per-class rate axis matching layer 1 exactly, and one row per rate epoch:

| Field                                                                                                                                                     | Notes                                                                                                                                                                |
| --------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `driver`, `model_canonical`                                                                                                                               | Join key from `work_run_model_usage`.                                                                                                                                |
| `effective_from`, `effective_to`                                                                                                                          | Half-open interval. A run is priced with the epoch containing its `work_runs.started_at`, so July tokens are priced at July rates regardless of when the query runs. |
| `fresh_input_rate`, `cache_read_rate`, `cache_write_5m_rate`, `cache_write_1h_rate`, `cache_write_unsplit_rate`, `output_rate`, `reasoning_separate_rate` | Per-token rates, one per disjoint class. A NULL rate for a class means "this model does not bill that class" and is distinct from a zero rate.                       |
| `source_url`, `source_retrieved_at`                                                                                                                       | Provenance. Every row records where its numbers came from and when they were read.                                                                                   |

**No numbers appear in this design.** They are read from the provider's published pricing page at implementation time and recorded with their source. A rate that cannot be cited is not entered.

The failure policy is the important half:

- A run whose `model_canonical` is NULL, or whose `(driver, model, date)` key has no rate row, is **`unpriceable`**. It is never priced at zero and never silently dropped from a sum.
- A figure computed over a set containing unpriceable runs reports **both** the priced subtotal **and** the unpriceable count, and is labelled partial. It never presents the priced subtotal alone.
- `cache_write_unsplit_tokens > 0` against a model with no `cache_write_unsplit_rate` makes that run unpriceable rather than guessing a TTL.
- Unpriceable causes are enumerated, not collapsed — `unknown_model`, `no_rate_epoch`, `unsplit_cache_write`, `capture_skipped` are separately countable, so a growing bucket is diagnosable rather than merely alarming.

Rate-table changes are a human decision with money attached. The table is version-controlled and reviewed, not runtime-editable — see the open question on this.

### Layer 4 — Roll-up semantics

Three decisions the data cannot make for us.

**Attribution rule (non-negotiable foundation).** Every execution's spend attributes to its own `work_item_id`, always, with no cross-row re-attribution ever written to the database. Re-attribution exists only as a _view_. This is what keeps board-level sums well-defined.

Two views, always distinctly labelled:

- **`direct`** — the row's own executions only. **Board-level totals sum `direct`, and only `direct`.** This is the sum that is provably free of double-counting.
- **`inclusive`** — `direct` plus the `direct` of `parent_task_id` descendants (chains are one level deep, verified). This is the honest answer to "what did this chore cost me, all in", and it is the right default for a _single row's_ detail view.

**Do revision children roll into the parent? Yes — in `inclusive` only, never in a board sum.** Revision children hold ~20% of cache-read and ~21% of output relative to parent-kind rows; ignoring them under-counts a chore by that much, and summing `inclusive` across a board double-counts exactly that slice. Naming both views and forbidding board-level `inclusive` summation is what lets both facts be true at once. The recommendation is that a row's detail view leads with `inclusive` and shows `direct` beside it; a board or list view shows `direct` only.

**Do `pr_review` executions count? Yes as spend, but never blended into one number.** Executions bucket into three spend classes by `ExecutionKind` (`protocol/src/types/execution.rs:44`):

| Class            | Kinds                                                                                                                                             |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `implementation` | `task_implementation`, `chore_implementation`, `revision_implementation`, `investigation_implementation`, `ci_remediation`, `conflict_resolution` |
| `design`         | `project_design`, `product_design`                                                                                                                |
| `overhead`       | `pr_review`, `automation_triage`, `answer_agent`                                                                                                  |

Every figure reports the class split alongside the total. `pr_review` is real money spent on that work item — 118 runs, 140M cache-read — and excluding it would under-report. But it is not implementation spend, and a single blended figure would make an expensive review indistinguishable from an expensive implementation. The split costs one extra column and removes the ambiguity entirely.

**How is low coverage surfaced?** Every figure carries a **coverage triple** — `executions_total`, `executions_with_usage`, `executions_priceable` — and a derived confidence:

- **`none`** — `executions_priceable == 0`. The system shows **no figure**: an em dash plus the reason (pre-instrumentation, capture skipped, unpriceable model). It never shows `$0.00`. Rows with 109 and 74 executions and zero tokened runs land here, which is the correct outcome.
- **`partial`** — `0 < executions_priceable < executions_total`. The figure is shown, always with the ratio adjacent and always labelled as a floor, never as a total.
- **`complete`** — every execution priced.

A row with 287 executions and 3 tokened runs renders as `partial`, `3/287`, and a figure explicitly described as a lower bound. That is the honest refusal the project asks for, expressed as a presentation invariant rather than as a caveat someone has to remember to write.

### Layer 5 — Read surface

- **`bossctl cost show <selector>`** — per-work-item: the coverage triple, confidence, class split, `direct`/`inclusive`, and per-model token breakdown. `--json` for machine use.
- **`bossctl cost runs <selector>`** — per-run drill-down, including `cost_capture_status` and unpriceable causes. This is the "why is this row partial" answer.
- **`bossctl cost rates`** — the effective rate table with provenance and dates, so an operator can see what a figure was computed against.
- **`boss task show --json`** — gains a `cost` sub-object with the same shape. This is the wire surface most downstream consumers will actually read, and today `work_runs` is not on it at all.
- **macOS app** — deferred. The CLI surface answers the operator question; the app surface is a presentation project with its own design questions and should not gate this one.

### What the system shows where it cannot attribute

Stated explicitly, because "honest refusal" is a deliverable and not a disposition:

| Situation                                           | What is shown                                                                                                                       |
| --------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Pre-instrumentation row (~4017 rows)                | `—` with `not instrumented — this work predates cost capture (2026-07-26) and cannot be recovered`. Permanent.                      |
| Capture skipped (`cost_capture_status != captured`) | `—` or `partial`, with the specific skip reason named. Never conflated with zero usage.                                             |
| Unpriceable model                                   | Priced subtotal plus `N runs unpriceable (unknown_model: <raw string>)`. The raw provider string is shown so the gap is actionable. |
| No rate epoch for the run's date                    | Same, with `no_rate_epoch` and the date.                                                                                            |
| Genuinely zero usage                                | `$0.00`, with confidence `complete`. This is the _only_ path to a displayed zero.                                                   |

## Risks / open questions

- **The Codex containment relation is assumed, not proven.** `fresh_input = input − cached − cache_write` is the natural reading, but the only in-repo sample has a zero cache-write. Shipping the Codex extractor on an unconfirmed relation would re-create defect 2 in a new form. Mitigation: the investigation task is sequenced _before_ the extractor, and the extractor treats a negative result as a hard error rather than a clamp.
- **Reasoning-token inclusion is unresolved for Codex and Grok.** Grok's `reasoningTokens: 24` against `outputTokens: 33` is consistent with containment but proves nothing. If reasoning is additive and we assume it is inside output, every reasoning-heavy run is under-priced. Sequenced as its own investigation.
- **The hook-target resolver may split one execution's cost across two rows.** `resolve_run_id_for_execution_hooks` (`work/executions_runs.rs:2753`) orders by unfinished → non-failed → has-transcript → newest. If the agent-session run has `finished_at` set while a newer unfinished non-failed sibling exists, late hooks would land on the sibling. The brief verified zero cases of two runs of one execution sharing a `transcript_path`, which is evidence against this happening in practice — but **no test pins the ordering**, so nothing prevents a future change from introducing it. The proposed fix is to prefer the run whose `transcript_path` equals the advertised path before falling back to the existing order, plus a test. This wants confirmation before the fix lands; the test is worth writing either way.
- **Rate-table drift is a correctness risk with no automated detector.** A provider changes a rate, nobody updates the table, and every subsequent figure is quietly wrong while looking complete. The Grok `costUsdTicks` oracle detects this for one driver. There is no equivalent for Claude or Codex, and this design does not solve it — it flags it. A periodic human re-verification of `source_retrieved_at` staleness is the minimum viable answer.
- **Coverage will remain low for a long time.** Even with every defect fixed, only runs from the fix forward carry good data, against 4017 rows that never will. The board will show mostly `—` for months. This is correct behaviour and should not be mistaken for the feature failing.
- **`cache_write_unsplit` may prove to be the common case for Codex**, which reports no TTL split at all. If the provider's actual billing collapses to a single cache-write rate, an `unsplit` rate entry resolves it cleanly. If it does not, most Codex runs are unpriceable and that needs a human decision rather than a default.
- **Per-model attribution assumes the provider names the model on the usage-bearing record.** True for Claude (`message.model`) and Grok (`modelUsage`). For Codex the model arrives on a separate `turn_context` record and must be associated by position in the stream, which is a weaker binding — a mid-run model switch could misattribute. Worth confirming whether Codex can switch models mid-session at all.

## Proposed implementation task breakdown

Dependency depths are marked. Entries at the same depth may run in parallel unless a file-overlap note says otherwise.

---

**1. Persist capture provenance on `work_runs` (`driver`, `cost_capture_status`)**

Add two columns via a new migration in the engine's migration chain, assert them in `schema_init`, and set them on the existing idempotent snapshot path in `work/executions_runs.rs`. `driver` is sourced from the existing `driver_transcript::resolve_execution_driver_slug` (reuse, not a new resolution path) and snapshotted at first capture. `cost_capture_status` records `captured` / `skipped_containment_unresolved` / `skipped_no_transcript` / `no_usage_observed`, set from the branch in `app/worker_events.rs` that currently skips capture silently. No behaviour change beyond recording; this unblocks everything else and makes the coverage question measurable.

- Effort: `small`
- Depends on: none
- Scope: in-scope

---

**2. Pin the hook-target run resolver with a test, and prefer transcript-path match**

Add tests to `work/executions_runs.rs` covering `resolve_run_id_for_execution_hooks` with a finished agent-session run alongside a newer unfinished non-failed sibling, confirming or refuting the suspected split. If confirmed, prefer the run whose `transcript_path` equals the hook's advertised path before falling back to the existing unfinished → non-failed → has-transcript → newest order. If refuted, keep the tests as regression pins.

- Effort: `small`
- Depends on: entry 1 (edits the same file and migration-adjacent code; land after so entry 1's changes are forward-ported preservingly rather than conflicted)
- Scope: in-scope

---

**3. Investigation: cross-driver token vocabulary reconciliation**

Study real Claude transcripts, Codex rollouts, and Grok `updates.jsonl` captures to establish, per driver and with cited evidence: whether `input` is gross or net; whether cache-write tokens are contained in `input`; whether reasoning tokens are inside `output` or additive; and whether Codex can switch models mid-session. Produce a written mapping table from each driver's native fields into the disjoint vocabulary (`fresh_input`, `cache_read`, `cache_write_5m/1h/unsplit`, `output`, `reasoning_billed_separately`), with the arithmetic identity that validates each. Deliverable is a document under `tools/boss/docs/investigations/`, not code. This gates every extractor; shipping one on an unconfirmed relation re-creates defect 2 in a new form.

- Effort: `medium`
- Depends on: none (may run in parallel with entry 1)
- Scope: in-scope

---

**4. Add a driver-owned cost-extraction capability to `AgentDriver`, with the Claude implementation**

Define the canonical usage record and a new capability on the `AgentDriver` trait alongside the existing transcript-normalisation capability. Implement it for Claude as a behaviour-preserving move of the logic currently in `CostAccumulator::ingest_assistant`, and rewire `run_cost.rs` to call the driver instead of matching on record shape itself. `run_cost.rs` keeps its incremental tail, `message.id` dedup, and idempotent persistence, and stops knowing what an assistant record looks like. Existing `run_cost.rs` tests must pass unchanged.

- Effort: `medium`
- Depends on: entries 1, 3
- Scope: in-scope

---

**5. Codex cost extraction: gross-to-net normalisation**

Implement the extraction capability for Codex in `driver/src/codex.rs`, mapping `total_token_usage` into the disjoint vocabulary using the containment relation established by entry 3. A negative `fresh_input` is a hard error that marks the run unpriceable, never a clamp to zero. Preserve the existing cumulative-versus-incremental handling (`total_token_usage` is a running total and must replace, not accumulate).

- Effort: `small`
- Depends on: entry 4
- Scope: in-scope

---

**6. Grok cost extraction from `updates.jsonl` `turn_completed`**

Implement the extraction capability for Grok. The `turn_completed` record is already parsed and its `usage` object discarded at `grok/transcript.rs:138`; read it there. Map `inputTokens`/`cachedReadTokens` gross-to-net, and prefer the per-model `modelUsage` map over the flattened totals so entry 7 receives a real breakdown. Capture `costUsdTicks` into a column for later oracle validation; do not use it as the cost figure.

- Effort: `small`
- Depends on: entry 4
- Scope: in-scope
- Parallel with entry 5 — different driver files, no overlap.

---

**7. Per-model usage breakdown table**

Add the `work_run_model_usage` child table and write one row per `(run_id, driver, model_raw)` from the extraction results, replacing the run-level last-writer-wins model field as the pricing source. Retain `work_runs.model` as a denormalised display hint set to the model with the most output tokens, so existing readers keep working. This is the fix for defect 4: `<synthetic>` becomes its own row holding its own (negligible) usage instead of overwriting a real label.

- Effort: `medium`
- Depends on: entry 4
- Scope: in-scope
- Parallel with entries 5 and 6 in principle, but it edits `run_cost.rs`'s persistence path, which they also touch. Land entry 7 **after** 5 and 6, forward-porting their extractors into the new write path preservingly.

---

**8. Driver-owned model-slug canonicalisation**

Add a canonicalisation function to each driver's `ModelMenu` mapping an observed model string to a canonical billable id (`opus` → `claude-opus-5`, `grok-4.5-build` → its canonical form), and populate `model_canonical` from it. Unrecognised strings leave `model_canonical` NULL and retain `model_raw` verbatim. Sentinel non-models (`<synthetic>`) canonicalise to NULL by design.

- Effort: `small`
- Depends on: entry 7
- Scope: in-scope

---

**9. Claude subagent transcript enumeration**

When the driver declares its transcripts carry sidechains, enumerate the sibling `subagents/` directory alongside the advertised parent transcript path and fold those files into the same accumulator. Derive the paths from the parent path rather than adding a `SubagentStop` hook, so no change to worker settings is needed. Rely on the existing `message.id` dedup (ID spaces are disjoint, so this is purely additive). Enumeration failure sets `cost_capture_status = partial_subagents_unavailable` rather than failing silently. Recovers the measured ~25.4% of fresh input and ~12.4% of cache-creation currently uncounted.

- Effort: `medium`
- Depends on: entry 7
- Scope: in-scope

---

**10. Investigate and close the Codex capture-coverage gap**

Using `cost_capture_status` from entry 1, measure why Codex post-instrumentation coverage sits at 25.9% versus Claude's 99.0%, confirming or refuting the containment-refusal suspicion at `app/worker_events.rs:379`. Fix the root cause. Degrading a contained driver to an unrestricted tail is **not** an acceptable fix — the refusal is the correct security posture, and the fix must make containment resolve correctly or make the skip loud, never make the read unsafe.

- Effort: `medium`
- Depends on: entries 1, 5
- Scope: in-scope

---

**11. Reasoning-token accounting for Codex and Grok**

Implement whatever entry 3 establishes: if reasoning tokens are additive, populate `reasoning_billed_separately` from `reasoning_output_tokens` / `reasoningTokens` and give the rate table a matching class; if they are contained in output, add an assertion and a test documenting the containment so nobody re-opens it. Either way the outcome is pinned by a test rather than an assumption.

- Effort: `small`
- Depends on: entries 3, 5, 6
- Scope: in-scope

---

**12. Golden-corpus conformance sweep for cost extraction**

Extend the existing driver conformance goldens with captured usage-bearing records from all three drivers, asserting each extractor produces the expected disjoint vocabulary and that the class sums reconcile against each provider's own reported totals. A sweep over the captured investigation artifacts, run as a test — separate from the extractors it validates.

- Effort: `medium`
- Depends on: entries 5, 6, 9, 11
- Scope: in-scope

---

**13. Rate table: schema, loader, and effective-date resolution**

Introduce a small crate (per the repo's prefer-crates-over-modules convention) owning the rate table: its schema keyed on `(driver, model_canonical, effective_from, effective_to)` with one rate per disjoint token class, the loader, provenance fields (`source_url`, `source_retrieved_at`), and the date-resolution logic that selects the epoch containing a run's `started_at`. Ships with **no rate values** and an empty table — populating it is entry 14.

- Effort: `medium`
- Depends on: entry 8
- Scope: in-scope

---

**14. Populate the rate table from authoritative published pricing**

Data-only change. Read each provider's published pricing page at implementation time and enter one row per `(driver, model, rate epoch)` with `source_url` and `source_retrieved_at` recorded. **Do not use recalled or inferred rates** — a rate that cannot be cited to a retrieved source is not entered, and the model is left unpriceable instead. Include historical epochs back to 2026-07-26 where the provider publishes them, so July tokens price at July rates.

- Effort: `small`
- Depends on: entry 13
- Scope: in-scope

---

**15. Pricing engine: priced usage and unpriceable classification**

Compute `Σ (class_count × class_rate)` over `work_run_model_usage` joined to the rate table on `(driver, model_canonical, run date)`. Classify every unpriceable run by enumerated cause — `unknown_model`, `no_rate_epoch`, `unsplit_cache_write`, `capture_skipped` — and never price an unpriceable run at zero or drop it from a sum. Returns a priced subtotal plus an unpriceable breakdown, never a bare scalar.

- Effort: `medium`
- Depends on: entries 13, 7
- Scope: in-scope

---

**16. Roll-up query layer: direct/inclusive views, spend classes, coverage triple**

Implement the attribution rule (every execution attributes to its own `work_item_id`; re-attribution exists only as a view), the `direct` and `inclusive` views over `parent_task_id`, the three-way `implementation`/`design`/`overhead` split by `ExecutionKind`, and the coverage triple with its derived `none`/`partial`/`complete` confidence. Enforce as an invariant that board-level totals sum `direct` only.

- Effort: `medium`
- Depends on: entry 15
- Scope: in-scope

---

**17. Protocol: expose cost on the wire**

Add the cost types to `boss-protocol` — coverage triple, confidence, class split, per-model breakdown, unpriceable causes — and the RPC to fetch them for a work item and for its runs. Pure protocol/plumbing change with no presentation.

- Effort: `small`
- Depends on: entry 16
- Scope: in-scope

---

**18. `bossctl cost` verbs (`show`, `runs`, `rates`)**

Add a `CostCommand` enum and its handlers: `cost show <selector>` (coverage, confidence, class split, direct/inclusive, per-model breakdown), `cost runs <selector>` (per-run drill-down with `cost_capture_status` and unpriceable causes), `cost rates` (effective rate table with provenance and dates). Text and `--json` output. Enforce the presentation invariants: confidence `none` renders an em dash and a reason, never `$0.00`.

- Effort: `medium`
- Depends on: entry 17
- Scope: in-scope

---

**19. `boss task show --json` cost block**

Add a `cost` sub-object to the task-show payload with the same shape as entry 18's `--json`. This is the wire surface most downstream consumers read, and `work_runs` is absent from it entirely today.

- Effort: `small`
- Depends on: entry 17
- Scope: in-scope
- **Substantial file overlap with entry 18** — both add to `cli/src/commands.rs` (command enums) and the shared output helpers. Land entry 18 first; entry 19 must forward-port entry 18's changes preservingly rather than replacing them.

---

**20. Grok `costUsdTicks` oracle validation sweep**

Diff Boss's computed cost against Grok's provider-reported `costUsdTicks` across every Grok run carrying both, and report systematic divergence. This is the only end-to-end accuracy check available anywhere in this system — a divergence is direct evidence that the rate table or the token vocabulary is wrong. A validation campaign, sequenced after the implementation it validates.

- Effort: `small`
- Depends on: entries 6, 14, 15
- Scope: in-scope

---

**21. macOS app cost surface**

Surface per-work-item cost, coverage, and confidence in the Boss app, honouring the same presentation invariants as the CLI.

- Effort: `medium`
- Depends on: entry 17
- Scope: deferred (future / not a v1 blocker) — the CLI surface answers the operator question, and the app presentation carries its own layout and information-density decisions that would gate this project on an unrelated design.

---

**22. Rate-staleness detection for Claude and Codex**

Detect that a provider changed a published rate while the table still holds the old one — the failure mode where every figure is quietly wrong while looking complete. The Grok oracle (entry 20) covers one driver; there is no equivalent for the other two, and the minimum viable answer is a staleness warning driven off `source_retrieved_at`.

- Effort: `small`
- Depends on: entry 14
- Scope: deferred (future / not a v1 blocker) — a manual re-verification cadence is adequate at current volume; automate once the table has enough epochs for drift to be plausible.

---

**23. Backfill of pre-instrumentation runs — permanently rejected, do not schedule**

Listed only so that it is visibly considered and closed rather than silently omitted. The ~4017 pre-instrumentation `in_review`/`done` rows **cannot** be backfilled: Boss stores a pointer into the provider's transcript directory, and Claude and Codex have both reclaimed those files under their own retention. The bytes do not exist. The same applies to the 313 of 331 runs whose subagent transcripts are already gone. **This entry must never be materialised into a task.** The correct handling is entry 16's confidence `none` with a "not instrumented" reason on those rows.

- Effort: `trivial`
- Depends on: none
- Scope: deferred (future / not a v1 blocker) — recorded as permanently rejected, not as pending work; there is no future in which the source data returns.

---

### Parallelism summary

| Depth | Entries  | Notes                                                                                             |
| ----- | -------- | ------------------------------------------------------------------------------------------------- |
| 0     | 1, 3     | Fully parallel — different subsystems (engine schema vs investigation doc).                       |
| 1     | 2, 4     | 2 depends on 1; 4 depends on 1 and 3.                                                             |
| 2     | 5, 6     | Fully parallel — separate driver files.                                                           |
| 3     | 7        | Serialised after 5 and 6: shares `run_cost.rs` persistence path.                                  |
| 4     | 8, 9, 10 | Parallel; 10 also needs 5. 8 and 9 touch different layers (canonicalisation vs tail enumeration). |
| 5     | 11, 13   | Parallel.                                                                                         |
| 6     | 12, 14   | Parallel — 12 is a test sweep, 14 is data.                                                        |
| 7     | 15       | —                                                                                                 |
| 8     | 16, 20   | Parallel — 20 is validation, 16 is the query layer.                                               |
| 9     | 17, 22   | Parallel.                                                                                         |
| 10    | 18, 21   | 21 deferred.                                                                                      |
| 11    | 19       | Serialised after 18: substantial CLI file overlap.                                                |
