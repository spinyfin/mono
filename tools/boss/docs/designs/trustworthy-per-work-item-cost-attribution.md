# Boss: Trustworthy per-work-item cost attribution

- **Date:** 2026-07-30. **Revised:** 2026-09-02 (second review pass; see the revision note directly below).
- **Status:** Design — awaiting review. No implementation in this change.
- **Product:** Boss
- **Deliverable:** this document. The task breakdown at the end is the handoff to scheduling.
- **Evidence base:** original findings from the project brief (`spinyfin/mono` @ `90e6c84b`, with live DB measurements taken 2026-07-29 20:15 CDT), first re-verified against `dea8ea14` on 2026-07-30. **This revision re-verified every code claim against `main` @ `d359999e0ca3f0f6a12a062390faf258577f8000` on 2026-09-02.** Every `file:line` below is against that commit. DB-population claims (row counts, coverage percentages, observed model strings) are carried from the 2026-07-29 brief and are labelled as such: this pass had no access to the production database and did not re-measure them. Treat them as a five-week-old snapshot.
- **Related:** [`codex-as-a-first-class-agent-driver.md`](codex-as-a-first-class-agent-driver.md), [`grok-as-a-first-class-interactive-agent-driver.md`](grok-as-a-first-class-interactive-agent-driver.md), [`agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md`](agent-driver-abstraction-decouple-boss-from-claude-code-capabilities-oriented-mix-and-match.md), [`antigravity-driver-fourth-driver-google-gemini-cli.md`](antigravity-driver-fourth-driver-google-gemini-cli.md), [`engine-counter-metrics-framework.md`](engine-counter-metrics-framework.md)

## Revision note, 2026-09-02: what changed, and whether the plan changed

Roughly five weeks and ~275 commits landed on `main` between the original evidence base and this pass. The four capture defects are **all still present, unchanged, in `run_cost.rs`**. The design's core move (driver-owned extraction into one disjoint vocabulary, per-model attribution, pricing strictly on top) still holds and is, if anything, better supported by what has since been built. But four things landed that materially change the _framing_ and the task breakdown, and one of them changes a conclusion the original stated as fact. In decreasing order of consequence:

1. **A read surface and a pricing table now exist, and they price on the defective vocabulary.** `boss cost task|window|top` landed in #2596 on 2026-07-31, one day after this design was written. It aggregates the nine `work_runs` columns as-is (`engine/core/src/cost_report.rs`, `work/cost_report_db.rs`, `app/cost.rs`) and, under `--usd`, prices them from a **hardcoded, uncited, undated** in-code table (`engine/core/src/cost_pricing.rs:31`, `MODEL_PRICING`) matched by **substring** against the raw `work_runs.model` string. The original's "there is no rate table and no read surface" is therefore no longer true, and Layers 3 and 5 are no longer greenfield: they are a hardening of an existing surface. The landed surface is honest in the ways that were cheap (NULL is never summed as zero; unknown models are excluded and named; windows spanning the capture boundary are flagged) and inherits every defect that was not: defect 2's gross/net collision (latent today only because no Codex or Grok model is in the table), defect 4's `<synthetic>` and alias labels (each becomes its own bucket), the flat cache-write rate that ignores the 5m/1h split the capture layer went to the trouble of recording, and a `total_tokens` ranking in `boss cost top` that adds gross Codex input to its cached portion. **Recommendation: the design's Layers 3 and 5 are re-scoped as extend-and-correct, and a new entry (24) gates `--usd` behind the vocabulary fix.** See "The landed `boss cost` surface" under Background.
2. **Boss now records an immutable launch tuple per execution — `work_executions.driver`, `.model`, `.effort_level`** (`work/migrations_b.rs:2785`, written once at spawn by `work/execution_launch_config.rs:8` from `coordinator/run.rs:204`) — plus a per-execution routing record, `execution_driver_decisions` (`migrations_b.rs:2660`, from #2590/#2616). This delivers most of what Layer 0's `work_runs.driver` proposed, at the execution grain rather than the run grain. **Layer 0 is revised to source its snapshot from the launch tuple** and to keep only `cost_capture_status` as a genuinely new column. It also refutes an original claim: "there is no fallback source" for the model. There is one now, `work_executions.model`, but it is the _requested_ alias (`opus`, `grok-4.6`, `gpt-5.6-sol`), not the observed provider string, so it is a fallback for display and never a substitute for canonicalisation (defect 4 stands).
3. **Codex and Grok transcripts are now Boss-owned and durable.** #2662 (2026-08-04) links each isolated home's `sessions/` directory into `<Boss state root>/executions/<run_id>/transcripts/<driver>/sessions` (`engine/driver/src/transcript_store.rs:35,79`). This is not a mirror; it is the primary file, relocated into Boss's own storage, and this pass found **no code path that reclaims that directory** (the Codex and Grok home-retention sweeps operate on the temporary home containers; `execution_retention` prunes DB rows). Two consequences. The "no Boss-internal transcript mirror" non-goal is overtaken by events for two of three drivers and is rewritten below. And the "backfill is permanently impossible" conclusion **now needs a caveat**: for Codex and Grok runs from 2026-08-04 forward, a capture fix _can_ be followed by a bounded re-extraction, because the bytes still exist. Claude transcripts remain under `~/.claude/projects` and Claude Code's own cleanup, so the pre-instrumentation rows and every Claude run remain unrecoverable exactly as stated. Entry 23 is split accordingly and a new entry (25) covers the bounded re-extraction.
4. **Driver traffic allocation** (#2590, #2616, #2625) means a work item with no explicit pin is now placed on `grok`, `claude`, or `codex` by a hash of its id against an operator-controlled three-way split (`work/driver_allocation.rs`). `tasks.driver` is NULL for that traffic by design. The original's argument that "re-deriving the driver later is not the same fact as the driver that ran" was made when the resolver was a two-step precedence chain; it is now a pool-override → live-pin-with-capability-gate → frozen-allocation → default chain (`work/driver_lookup.rs:60`). The argument is stronger, not weaker, and the launch tuple in item 2 is the right answer to it.

Smaller drifts, each recorded in place below: the driver crate moved from `tools/boss/driver` to `tools/boss/engine/driver`; `set_run_cost_snapshot` and the hook-target resolver moved from `work/executions_runs.rs` to `work/run_rows.rs`; the transcript tail moved to its own `engine/transcript-tail` crate; the Codex driver retired `codex exec` for a persistent TUI (#2578) and now feeds the cost capture site through the file-tail ingress rather than a hook payload; Grok dispatches `grok-4.6` (#2764) rather than the `grok-4.5-build` in the sampled telemetry; the resolver-ordering risk now has partial test coverage; a fourth driver (Antigravity, `agy`) has a design that explicitly defers cost accounting to a cross-driver mechanism, which this design is; and a **provider quota** surface (#2779, `protocol/src/types/driver_quota.rs`) landed that its own docs correctly refuse to conflate with Boss-side accounting.

**Net: the conclusion stands; the plan's shape changes from "build five layers" to "fix capture, then correct and extend the surface that already ships."** The number of entries grows by two (24, 25); five existing entries are re-scoped against code that now exists.

## Verdict

Boss captures nine columns of token telemetry per run and cannot turn any of it into a cost figure a person should believe. The blocker is not arithmetic — it is that the `(driver, model, disjoint-token-class, count)` tuple pricing requires **does not exist in the database today**, for four independent reasons. Since 2026-07-31 there _is_ a rate table and a read surface (`boss cost`), and their existence sharpens the problem rather than solving it: a figure with a dollar sign now reaches an operator, computed over exactly the tuple this document says is wrong.

The design's core move is to stop treating cost as a nine-column widening of `work_runs` and start treating it as **two normalised facts recorded at capture time** — the driver that produced the run, and a per-model breakdown of _disjoint_ token classes — with pricing, roll-up, and presentation layered strictly on top. Everything downstream of that is mechanical. Everything upstream of it is currently wrong in a way that no downstream cleverness can repair. The landed `boss cost` surface is downstream cleverness, and it is good downstream cleverness; it is priced over the wrong upstream.

The secondary move is to make **absence loud**. Every figure the system shows carries a coverage triple, and a row that cannot be priced shows why, never a zero. `boss cost` already does two-thirds of this (it distinguishes unmeasured from zero and names unpriced models); the missing third is "captured but wrong", which is invisible today because nothing records that a capture was skipped, partial, or on an unnormalised vocabulary.

## Goals

- A per-work-item spend figure the operator can believe, **or an honest refusal** where the data does not support one. A silently wrong figure is worse than no figure; this is the constraint the whole design is shaped around.
- Close the four capture defects so that runs recorded _from the fix forward_ carry the tuple pricing needs.
- Give model attribution that **survives a run** — recorded at capture time from the transcript, not re-derived later from mutable rows.
- Decide and defend the roll-up semantics (revision children, `pr_review` executions, board-level summation) rather than leaving them to whoever writes the first `SUM()`. (`boss cost task` has since written the first one; it is `direct`-only and that is the right choice, but nothing labels it as such.)
- Expose the result on a read surface, so that answering "what did this cost" stops requiring hand-written SQL against `state.db`. Partly delivered by #2596; the remainder is making what it shows trustworthy.
- Make coverage a first-class part of every answer, so a row with 287 executions and 3 tokened runs cannot present itself as authoritative.

## Non-goals

- **Backfilling the ~4017 pre-instrumentation `in_review`/`done` rows. This is permanently impossible and must not be re-opened.** Boss stores only `work_runs.transcript_path`, a pointer into the provider's own directory. For every one of those rows the provider was Claude Code, whose transcripts live under `~/.claude/projects` and are reclaimed by its own ~30-day cleanup; nothing Boss does can recover a token count from a deleted JSONL file. The historical board is permanently uncosted; the design's answer is to _say so on those rows_, not to invent a number for them. **Revision caveat:** this argument is about Claude transcripts and pre-instrumentation rows. Since 2026-08-04 (#2662) Codex and Grok session files live under Boss's own state root and this pass found no reclaim path for them, so a _bounded_ re-extraction of post-instrumentation Codex/Grok runs after a capture fix is possible and is scoped as entry 25. That is not a backfill of the 4017 and must not be described as one.
- **A Boss-internal copy of Claude transcript content to work around Claude Code's retention.** The original rejected any Boss-owned transcript store on the grounds that it duplicates a large, sensitive artifact for a derived scalar. Events have partly overtaken that: #2662 made Boss the durable home of Codex and Grok session files, for reasons unrelated to cost (a killed or orphaned worker must not lose its transcript with its temporary home). The design does not propose extending that to Claude, whose transcript path is authoritative and lives outside any Boss-owned home, and does not propose _copying_ anything anywhere. It simply notes that where the durable store already exists, cost extraction may read from it.
- **Populating rate values in this document.** Model rates change and must come from the provider's published pricing at implementation time. This design specifies the _shape_ of the rate table, its provenance and effective-dating semantics, and its failure policy. It deliberately contains no numbers. Any figure recalled from a model's training data is not authoritative and must not enter the table. **The `MODEL_PRICING` constants in `cost_pricing.rs` carry no source and no retrieval date and so do not meet this bar; entry 14 re-sources or removes them.**
- **Budget enforcement, spend caps, or dispatch gating on cost.** This project produces a trustworthy read. Acting on it — refusing to dispatch an expensive row, alerting on a runaway worker — is downstream work that this design makes possible but does not attempt.
- **A general time-series or cost-over-time analytics surface.** The unit of answer here is "what has this work item cost", not "plot org spend by week". `boss cost window` already exists and is a reasonable start on the latter; per-run rows retain their timestamps, so a later analytics layer is not foreclosed. It is just not built here.
- **Pricing non-token spend.** Web search calls, code execution, and similar metered provider surfaces are not captured today and are out of scope. Where a provider bundles them into a reported total, the design's oracle check (below) will surface the discrepancy rather than hide it.
- **Provider subscription quota.** The `driver_quota` surface (#2779) reports what each provider says is left of a subscription window. Its module doc states, correctly, that it "must never be presented as interchangeable" with Boss's own accounting: Boss sees only the work Boss dispatched; the provider sees the whole subscription. This design does not consume it and does not feed it.

## Background: what exists, and the four ways it is wrong

Instrumentation landed 2026-07-26 (commit `f01a5b3a6`). Nine columns hang off `work_runs` (`model`, `input_tokens`, `output_tokens`, `cache_creation_tokens`, `cache_read_tokens`, `cache_creation_5m_tokens`, `cache_creation_1h_tokens`, `rounds`, `agent_active_ms`), added by `migrate_work_runs_cost_columns` (`work/migrations_a.rs:525`; was `:445`) and asserted in `work/schema_init.rs:1003-1020`. Capture lives in `engine/core/src/run_cost.rs`, driven from a single production call site in `app/worker_events.rs:553` (was `:381-399`), persisted through `work/run_rows.rs:194::set_run_cost_snapshot` (moved from `work/executions_runs.rs`). The incremental file tail it sits on is now its own crate, `engine/transcript-tail`.

The existing capture machinery is in better shape than the raw defect list suggests, and the design should preserve rather than replace it. Re-read at `d359999e`, all of the following still hold: it dedupes Claude's multi-record responses by `message.id` (`run_cost.rs:137-152`); it treats Codex's cumulative `total_token_usage` as a replacement keyed by transcript path rather than an increment (`:156-178`); it refuses to invent a TTL split it did not observe (`cache_creation_ttl_split_known`, `:134` and `:175`); it persists before any completion gate, so an orphaned run keeps the spend observed up to its last hook (`worker_events.rs:532-537`); and its writes are assignments, not increments, so retried hooks and post-restart rebuilds are idempotent (`run_rows.rs:185-193`). The incremental-tail design is sound. **What is wrong is the vocabulary it accumulates into and the provenance it fails to record.**

One thing about the call site has changed and is worth knowing before reading the defects. The capture site is reached by whatever produces an `IncomingHookEvent` with a transcript path. For Claude that is still the hook payload (`events_socket.rs:363` → `ClaudeDriver::transcript_path_for_session`). For Codex it is **no longer a hook**: `CodexDriver::transcript_path_for_session` returns `None` unconditionally (`engine/driver/src/codex.rs:1889`), and the path instead arrives from the file-tail progress ingress that watches the durable rollout directory (`agent_jsonl_progress.rs:855-860`, canonicalised to the durable store). For Grok it is the hook payload's `transcriptPath` (`engine/driver/src/grok.rs:665`). All three converge on the same `worker_events.rs` block, so there is still exactly one capture site; there are now three ways to reach it.

### Defect 1 — Grok records nothing

**Status at `d359999e`: unchanged. Still open.**

`CostAccumulator::ingest` (`run_cost.rs:68`; was `:69`) matches on a top-level `type` field with arms for `assistant`, `system`/`turn_duration`, `turn_context`, and `event_msg`. Grok's transcript is `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl`, an ACP stream whose records are `{"timestamp", "method", "params"}` — no `type` at any level (`engine/driver/src/grok/transcript.rs` parses `params.update.sessionUpdate`, never `type`). Every record falls through to `_ => {}` (`run_cost.rs:96`). The brief found zero grok slugs in `work_runs`; this pass could not re-measure that but nothing in the code has changed that would alter it.

Re-verification in July found the situation **better than the brief describes**, and that still holds and still materially shrinks the fix. The usage is in a record the Grok driver **already parses and then discards**. `grok/transcript.rs:181` (was `:138`) maps `sessionUpdate == "turn_completed"` into `AcpEnvelope::TurnCompleted` (`:141`), keeping only `stop_reason`. The sibling `usage` object it drops is (verbatim, from `docs/investigations/ghostty-grok-pane-viability-artifacts/ghosttykit_host/evidence/esc_interrupt/session_telemetry_excerpt.md`, captured against `grok-4.5-build`; Grok now dispatches `grok-4.6` per `grok.rs:102`, and this pass did not re-sample the record shape under it — treat the field names as **unverified for 4.6** until entry 3 confirms them):

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

Grok's transcript now lives in the durable store (`grok.rs:684` resolves containment through `verified_durable_sessions_dir`), so once an extractor exists, Grok runs from 2026-08-04 forward are re-extractable (entry 25).

### Defect 2 — gross and net input share one column

**Status at `d359999e`: unchanged in the capture layer. Still open. The coverage sub-finding is stale and must be re-measured.**

A live Codex rollout in-repo (`docs/investigations/codex-exit-code-surfacing-artifacts/probes/p1_short_nonzero/`, still present):

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

`13982 + 134 = 14116`, so `input_tokens` already contains the cached portion. Claude's `input_tokens` is the disjoint fresh-input count. Both are written to `work_runs.input_tokens` (`run_cost.rs:122` for Claude, `:166` for Codex — same `MessageUsage.input_tokens` field, no marker). Any formula of the shape `input × fresh_rate + cache_read × cached_rate` is correct for Claude and **double-charges the cached input of every Codex and every future Grok row** — measured at 92.8% of Codex input in the brief's live sample. `cost_pricing::estimate_usd` (`cost_pricing.rs:74`) is exactly that formula. It does not mis-price Codex rows today only because no Codex model family is in `MODEL_PRICING`; the moment someone adds a `gpt` entry, every Codex row is over-charged by roughly its cache-hit ratio, silently, under an "ESTIMATE" label.

The non-USD path is affected too: `boss cost top` ranks by `input + output + cache_creation + cache_read` (`cost_report.rs:307`), which for a Codex row counts cached input twice. A Codex run and a Claude run of identical real usage do not rank equal.

This cannot be repaired downstream, because the column does not record which convention produced it. **What has changed is that `work_runs` no longer needs its own `driver`: `work_executions.driver` now records the driver that reached the spawned worker** (`migrations_b.rs:2785`, `execution_launch_config.rs:8`). The original's concern — that `driver_transcript::resolve_execution_driver_slug` (`driver_transcript.rs:99`) derives its answer from mutable rows at call time — is sharper now that most traffic is allocated by hash with `tasks.driver` NULL, but the answer is already recorded at the execution grain. Layer 0 is revised to use it. Two caveats for the implementer: the launch tuple is write-once per execution (`execution_launch_config.rs:18`), so a second run of the same execution is assumed to share the first's driver; and the column is NULL on every execution created before the migration, which the coverage story must render as "not recorded", not as Claude.

The brief measured Codex coverage at only 25.9% of post-instrumentation runs versus Claude's 99.0%, and suspected the containment refusal at what is now `worker_events.rs:520-530`: when `transcript_containment_root()` errs, `containment_root` is `None` and the entire capture block (`:539`) is skipped with a `warn!` and nothing in the database. **That suspicion is neither confirmed nor refuted by this pass, and the measurement behind it predates every relevant change.** Since 2026-07-29 the Codex driver retired `codex exec` for a persistent TUI (#2578), moved its transcripts into the durable store (#2662), pointed its progress ingress at the resolved durable directory rather than the symlink (#2680, whose commit message says every Codex spawn had been failing progress ingress before that fix), and now resolves containment through the durable store (`codex.rs:1900`). Any of those could have moved the coverage figure in either direction. What is unchanged and still a defect: the skip is **silent and indistinguishable in the database from "this run used no tokens"** — and, since #2596, indistinguishable in `boss cost` output from a pre-instrumentation run, because both render as `unmeasured`. Refusing the read remains the correct security posture (degrading a contained driver to an unrestricted tail would be a regression and must not be the fix). Entry 10 is re-scoped to re-measure first.

### Defect 3 — subagent tokens are excluded

**Status at `d359999e`: unchanged. Still open, and now confirmed Claude-specific by measurement rather than by inference.**

Claude Code writes subagent transcripts to `~/.claude/projects/<slug>/<parent-session-uuid>/subagents/agent-<id>.jsonl`. Every record carries `isSidechain: true` and a full `usage` object, with **zero `message.id` overlap** with the parent transcript. Boss never tails them: no `SubagentStop` hook is wired (`CLAUDE_HOOK_EVENTS`, `engine/driver/src/claude.rs:174`; was `driver/src/claude.rs:161`; still seven events — `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `SessionEnd` — and not that one), so no hook ever advertises those paths, and `RunCostTail.tails` (`run_cost.rs:298`) only gains an entry from a _differing advertised path_ (`:304-318`). No code under `engine/core` references `isSidechain` or a `subagents/` directory.

The brief measured **25.4% of fresh input and 12.4% of cache-creation uncounted** over the 18 token-bearing runs whose subagent files survived — a lower bound, and large enough that it alone disqualifies the current figure. Carried from the brief; not re-measured.

Two properties make the fix safe. The accumulator dedupes by `message.id`, and the ID spaces are disjoint, so folding subagent records in is purely additive with no double-count risk. And the exposure is Claude-specific by construction: the Grok driver spawns with `--no-subagents` (`engine/driver/src/grok.rs:257`; was `driver/src/grok.rs:178`). That flag was **re-examined and deliberately kept** in #2700 (2026-08-10, `docs/investigations/grok-subagent-hook-attribution-2026-08-09.md`): a Grok subagent's `session_end` is payload-identical to the top-level session's and would flip a live worker to terminated. So the flag is now load-bearing for liveness, not just tidiness, and this design can rely on it staying.

Backfill here is _mostly_ impossible — only 18 of 331 runs still had their files at measurement time — so this is a forward-capture fix. The design does not propose a partial backfill of the 18.

### Defect 4 — `<synthetic>` clobbers the real model

**Status at `d359999e`: unchanged in the capture layer. Still open. One supporting claim corrected.**

`CostAccumulator.model` is last-writer-wins at two sites — the Claude assistant record (`run_cost.rs:114`; was `:106`) and the Codex `turn_context` record (`:92`) — while token counts sum across every record. Claude Code appends a trailing synthetic assistant record whose model reads `<synthetic>`; a verified transcript with 379 `claude-opus-5` records and one `<synthetic>` — last — produced a DB row labelled `<synthetic>`. The brief found 24 runs in this state, one carrying 64.7M cache-read tokens. Grepping `tools/boss` still confirms no Boss code emits that string as a model value; `cost_pricing.rs` now _recognises_ it (`:11`, `price_for_model("<synthetic>")` is `None`), which is the right downstream behaviour and does nothing about the real label being lost upstream.

Last-writer-wins is also wrong for a legitimate reason, not just a synthetic-record accident: a single run can genuinely span models (subagent transcripts show `claude-sonnet-5` and `claude-opus-5` mixed), and only one label survives.

**Corrected claim.** The original said "there is no fallback source — `tasks.model_override` is NULL/empty on all non-deleted tasks and `products.default_model` is empty on all products." Both fields still exist and the DB-population claim is carried; but there **is** a fallback source now: `work_executions.model`, the model string the spawn resolver handed the worker (`protocol/src/types/execution.rs:670`, written by `record_execution_launch_config`). It is the _requested_ alias (`opus`, `sonnet`, `grok-4.6`, `gpt-5.6-sol` — see `claude.rs:63-109`, `grok.rs:102`, `codex.rs:257`), not the provider's observed string, and it is per-execution rather than per-record, so it cannot replace per-model attribution. It is a good display fallback for a run whose transcript yielded no model at all, and a good cross-check: a run whose observed model does not canonicalise to the same family as its launched model is an anomaly worth surfacing.

Observed `model` values across all 12,765 runs at measurement time (carried from the brief): `(NULL)` 12,418 · `claude-opus-5` 159 · `claude-sonnet-5` 142 · `<synthetic>` 24 · `gpt-5.6-terra` 14 · `opus` 1. The lone `opus` is a slug _alias_ — `claude_model_belongs_to_driver` (`claude.rs:139`; was `:128`) accepts bare `"opus"`, and drivers pass such aliases through to their CLI — reaching a Codex `turn_context.payload.model`. `cost_pricing::price_for_model` sidesteps the alias problem by substring-matching `"opus"` against the lowercased string (`cost_pricing.rs:62`). That is not canonicalisation; it is a heuristic that happens to work for three Anthropic family names and will silently match the wrong family the first time a provider ships a model name containing another provider's family word. Canonicalisation is required, and it belongs to the driver, which is the only component that knows its own model namespace.

### Not captured: reasoning tokens

**Status: unchanged.** Codex emits `reasoning_output_tokens`; Grok emits `reasoningTokens`. Neither is read by `run_cost.rs`. The only in-repo occurrences of either field are conformance fixtures and the driver's progress normaliser, which treats `token_count` as a benign bookkeeping envelope (`engine/driver/src/codex/progress.rs:384`). For Claude, thinking is billed inside `output_tokens`, so nothing is lost. For the other two, whether the field is additive or already inside `output` is **not answerable from the samples in this repo**: the Codex sample above has `reasoning_output_tokens: 0`, and the Grok sample has `reasoningTokens: 24` against `outputTokens: 33` — consistent with containment but far from proof. Assuming it is free is exactly the kind of silent error this project exists to prevent, so this is sequenced as an explicit investigation before any pricing depends on it.

### The landed `boss cost` surface (new since the original)

The original said: no rate table, no `bossctl` cost verb, nothing on the wire, hand-written SQL only. As of #2596 (2026-07-31):

- **CLI:** `boss cost task <id>`, `boss cost window --since [--until]`, `boss cost top --since [--limit]`, each with `--usd` and `--utc` (`cli/src/commands.rs:274,2899`; handlers in `cli/src/cost_cmds.rs`). Note the binary: these are `boss` verbs (the operator/worker CLI), not `bossctl` (coordinator-only). The original's Layer 5 named `bossctl` and was wrong to; corrected below.
- **Wire:** `FrontendRequest::GetWorkItemCostReport` / `GetCostWindowReport` / `GetCostTopReport`, with types in `protocol/src/types/cost_report.rs` (`CostMeasurement`, `TaskCostReport`, `WindowCostReport`, `TopCostReport`, `ExecutionCostRow`, `CostBucket`).
- **Aggregation:** pure functions in `engine/core/src/cost_report.rs` over `CostRunRecord` projections from `work/cost_report_db.rs`. `cost_records_for_work_item` (`:57`) selects `WHERE we.work_item_id = ?1` — the row's own executions only, i.e. this design's `direct` view, unlabelled.
- **Pricing:** `engine/core/src/cost_pricing.rs`. Three hardcoded `(family substring, USD per million)` entries for `opus`, `sonnet`, `haiku`; four classes (`input`, `output`, `cache_write`, `cache_read`); no driver key, no effective dating, no provenance fields, cache write priced flat with a comment declining to model the TTL split.
- **Honesty properties it has:** NULL token columns are `runs_unmeasured` and never summed as zero; a recorded zero is `runs_zero`; an unknown model is excluded from the estimate, sets `estimated_usd_partial`, and is listed in `pricing_gaps`; a window whose `since` predates `TOKEN_CAPTURE_START_EPOCH_S` (`cost_report.rs:27`, 2026-07-27T09:23:00Z) is flagged and its totals described as a floor; every USD figure is printed with "ESTIMATE, not billing truth"; a bucket with zero measured runs prints "unknown", not `$0.00`.
- **Honesty properties it lacks:** no distinction between "unmeasured because pre-instrumentation" and "unmeasured because capture was skipped" (both are NULL); no `priceable` count separate from `measured`; no `inclusive` view over revision children; no class split by execution kind on the per-task report (`by_kind` exists only on `window`); a `by_model` bucket keyed on the raw string, so `<synthetic>` and `opus` are buckets; and, as above, a pricing formula that is only correct for net-input drivers.

This is a well-built surface over the wrong tuple. The design's response is not to replace it but to fix what it reads and then extend it: Layers 3 to 5 below are rewritten as deltas against these files.

One more durability point in Boss's favour, unchanged: the execution retention prune (14 days, `work/execution_retention.rs:76-80`) excludes `completed` status by design (`:72-74`), so captured spend on completed runs persists indefinitely. Whatever is captured correctly from the fix forward stays captured.

## Alternatives considered

### A. Price at capture time — write a USD figure into `work_runs`

Compute cost in `run_cost.rs` as records are ingested and persist a `cost_usd` scalar alongside the token columns.

**Rejected.** It fossilises the answer. Rates change, and a stored figure computed under last month's rates cannot be re-derived or corrected. More damagingly, it fossilises _bugs_: every one of the four capture defects would have been baked into an immutable number that looks authoritative and carries no evidence of how it was produced. Tokens are the durable fact; dollars are a view over tokens and a dated rate, and must be computed on read. (#2596 agreed: it prices on read. Good.)

### B. Use the provider's own reported cost

Grok reports `costUsdTicks` per turn. Consume it directly and skip the rate table.

**Rejected as the primary mechanism.** Only Grok reports it — Codex's rollout and Claude's transcript carry no cost field — so a board would mix provider-authoritative Grok figures with Boss-computed Claude and Codex figures, which are not comparable and whose disagreements would be invisible. It also imports the provider's bundling decisions inconsistently across drivers, and provides no way to answer counterfactuals like "what would this row have cost on the cheaper model".

**Kept as an oracle.** Boss's computed Grok cost can be diffed against `costUsdTicks` on the same run. A systematic divergence is direct evidence that the rate table or the token vocabulary is wrong, and it is the only end-to-end accuracy check available anywhere in this system.

### C. Per-driver correction factor for the gross/net collision

Keep the single `input_tokens` column and apply a per-driver multiplier at pricing time to undo Codex's double-count.

**Rejected, and forbidden by the project brief.** A factor derived from an aggregate ratio is not a correction, it is a fudge that happens to be near-right on average and arbitrarily wrong per row — cache hit rates vary enormously between a fresh run and a resumed one. It also encodes a per-driver quirk in the _pricing_ layer, which is precisely the wrong place. The gross/net difference is a source-format difference and belongs to the component that already owns source-format differences — the driver.

### D. Wall-clock or `agent_active_ms` proxy

**Rejected.** Not a cost model; correlates with cost only through throughput. `agent_active_ms` remains useful as a _sanity_ signal, never as a price.

### E. Do nothing; document the columns and let operators write SQL

**Rejected**, and since #2596 moot: the SQL has been written and shipped as `boss cost`. The concern the original raised — that publishing the schema makes wrong figures _easier_ to produce — has materialised in the mildest available form (an optional, loudly-labelled estimate over three Anthropic families). It should be closed properly rather than left.

### F. Leave `boss cost --usd` as it is and build the corrected pipeline alongside (new)

Ship Layers 0 to 4 as a parallel path and let the existing `--usd` keep answering from `MODEL_PRICING` until the new one is ready.

**Rejected.** Two dollar figures for the same row, one of which is known to be over the wrong vocabulary, is worse than one. The existing estimate is correct for Claude-only rows today, and its limitations are exactly the ones this design closes, so the right move is to make it _refuse_ where the vocabulary is unnormalised (entry 24, small) and then upgrade it in place. Its pure-function structure (`cost_report.rs` builders over `CostRunRecord`) makes that an extension, not a rewrite.

## Chosen approach

Five layers, strictly ordered. Each is independently useful; each depends only on the ones above it. Where a layer now has a landed counterpart, the layer describes the delta.

```
  5. Read surface        boss cost {task,window,top} (exists) · + runs, rates · task show --json · (app, deferred)
  4. Roll-up             attribution rule (exists, unlabelled) · inclusive view · spend classes · coverage triple
  3. Pricing             cost_pricing.rs (exists, uncited, undated) → dated rate table · unpriceable classification
  2. Attribution         per-(model) usage breakdown · canonical model ids
  1. Capture             driver-owned extraction into one disjoint vocabulary
  0. Provenance          work_executions.{driver,model} (exists) · work_runs.cost_capture_status (new)
```

### Layer 0 — Record provenance at capture time

**Revised.** The original proposed two new columns on `work_runs`. One of them now exists at a better grain.

- **Driver** — `work_executions.driver`, the launch tuple written once at spawn (`work/execution_launch_config.rs:8`) from the resolved spawn config, is the fact the original wanted: the driver that reached the worker, frozen at that instant, immune to later edits of `tasks.driver` or to a change in the traffic split. The design uses it as the driver key for every run of that execution. `driver_transcript::resolve_execution_driver_slug` remains the fallback for executions that predate the column (NULL launch tuple), and a NULL with no resolvable fallback is a provenance gap that Layer 4's coverage story renders as `driver_unknown`, never as a guess. This is a strict reuse; no new resolution path.
- **`work_runs.cost_capture_status TEXT`** (new) — one of `captured`, `skipped_containment_unresolved`, `skipped_no_transcript`, `partial_subagents_unavailable`, `no_usage_observed`, `driver_unsupported`. This is what makes the difference between "this run genuinely used no tokens", "Boss declined to read this run", and "this run's driver has no extractor" visible in the data, and it is the piece `boss cost` cannot currently express (all three are NULL today). `driver_unsupported` is new in this revision: with a fourth driver designed (Antigravity, `agy`) whose design explicitly defers cost accounting to a cross-driver mechanism, the status must exist before the driver does, so that its runs are loudly uncosted rather than silently NULL.

Both are set on the same idempotent-assignment path as the existing snapshot, so retries and post-restart rebuilds behave identically to today. The set of statuses is closed and enumerated so that Layer 4 can count them.

### Layer 1 — One disjoint token vocabulary, normalised by the driver

**Unchanged in substance.** Define a canonical usage record whose fields are **mutually disjoint by construction**, so that a price is always `Σ (class_count × class_rate)` with no inclusion-exclusion anywhere:

| Class                         | Meaning                                                                                                                                                                                                                  |
| ----------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `fresh_input`                 | Input tokens billed at the full uncached rate. Contains no cached-read and no cache-write tokens.                                                                                                                        |
| `cache_read`                  | Input tokens served from cache.                                                                                                                                                                                          |
| `cache_write_5m`              | Cache-creation tokens at the 5-minute TTL.                                                                                                                                                                               |
| `cache_write_1h`              | Cache-creation tokens at the 1-hour TTL.                                                                                                                                                                                 |
| `cache_write_unsplit`         | Cache-creation tokens whose TTL the provider did not report. Priced only if the rate table declares an unsplit rate for that model; otherwise the run is unpriceable. Never silently assigned to a TTL.                  |
| `output`                      | Output tokens, inclusive of any reasoning tokens the provider bills inside output.                                                                                                                                       |
| `reasoning_billed_separately` | Reasoning tokens **only** where the provider bills them as an additive class outside `output`. Zero for Claude by definition. Populated for Codex and Grok only if the investigation task establishes they are additive. |

Extraction moves behind a new capability on `AgentDriver`, alongside the transcript capabilities the trait already carries (`normalize_transcript_entry`, `engine/driver/src/lib.rs:2081`; `transcript_session`, `:2053`; `transcript_containment_root`, `:2059`). Each driver maps its own dialect into the vocabulary above:

- **Claude** — already net. `fresh_input := input_tokens`, `cache_read := cache_read_input_tokens`, TTL split from `usage.cache_creation`. This is a behaviour-preserving move of today's `ingest_assistant` (`run_cost.rs:100-153`), which is what makes it a safe first extractor.
- **Codex** — gross. `fresh_input := input_tokens − cached_input_tokens − cache_write_input_tokens`. **The exact containment relation must be confirmed against real rollouts before this ships** — the in-repo sample has `cache_write_input_tokens: 0`, which does not distinguish "inside input" from "alongside input". A negative result from that subtraction is a hard error, not a clamp to zero. The durable store makes this investigation cheaper than it was: real post-2026-08-04 rollouts are on disk under Boss's state root.
- **Grok** — gross. `fresh_input := inputTokens − cachedReadTokens`, from the `turn_completed` record's `usage`, and preferentially from its per-model `modelUsage` map. Field names must be re-confirmed against `grok-4.6` output (entry 3); the sample is from `grok-4.5-build`.
- **Antigravity (`agy`)** — no extractor in v1. The driver declares no cost capability and every run records `driver_unsupported`. Its design already says cost accounting belongs to "a later mechanism [that] should compare all drivers under their actual subscription entitlements"; this is that mechanism's hook, left open for it.

A driver that does not implement the capability is not an error; it is a `driver_unsupported` status. That is the difference between this design and the current `_ => {}` arm.

`run_cost.rs` keeps its incremental tail, its `message.id` dedup, its cumulative-versus-incremental handling, and its idempotent persistence. It stops knowing what an assistant record or a rollout looks like.

Subagent enumeration (defect 3) attaches here: when the driver declares that its transcripts have sidechains, the tail additionally enumerates the sibling `subagents/` directory for the advertised parent transcript and folds those files into the same accumulator. This does not require a `SubagentStop` hook — it derives the paths from the parent path Boss is already told about. Directory enumeration failures degrade to `partial_subagents_unavailable`, not to silence.

### Layer 2 — Model attribution that survives the run

**Unchanged.** Usage is attributed to **the model named on the record that carried it**, not to a run-level last-writer-wins field. That requires a child table:

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

`model_canonical` comes from a **driver-owned canonicalisation** — the driver already owns its model namespace via `ModelMenu` and `model_belongs_to_driver`, and is the only place that knows `opus` and `claude-opus-5` are the same billable thing. It replaces `cost_pricing::price_for_model`'s substring heuristic, which should not survive Layer 3. Retaining `model_raw` verbatim alongside it is deliberate: when canonicalisation fails, the operator needs to see what the provider actually said.

The existing `work_runs.model` column is retained as a **denormalised display hint** — the model accounting for the most output tokens on that run — so existing readers (`boss cost`'s `by_model` buckets and `ExecutionCostRow.models`) keep working unchanged. Pricing never reads it. `work_executions.model` (the launched alias) is a second display hint and a cross-check, never a pricing key.

### Layer 3 — A dated rate table with a loud failure policy

**Revised: this is now a replacement of `cost_pricing.rs`, not a new thing.** A rate table keyed on `(driver, model_canonical, effective_from)`, with a per-class rate axis matching layer 1 exactly, and one row per rate epoch:

| Field                                                                                                                                                     | Notes                                                                                                                                                                |
| --------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `driver`, `model_canonical`                                                                                                                               | Join key from `work_run_model_usage`. Exact match; no substring matching.                                                                                            |
| `effective_from`, `effective_to`                                                                                                                          | Half-open interval. A run is priced with the epoch containing its `work_runs.started_at`, so July tokens are priced at July rates regardless of when the query runs. |
| `fresh_input_rate`, `cache_read_rate`, `cache_write_5m_rate`, `cache_write_1h_rate`, `cache_write_unsplit_rate`, `output_rate`, `reasoning_separate_rate` | Per-token rates, one per disjoint class. A NULL rate for a class means "this model does not bill that class" and is distinct from a zero rate.                       |
| `source_url`, `source_retrieved_at`                                                                                                                       | Provenance. Every row records where its numbers came from and when they were read.                                                                                   |

**No numbers appear in this design.** They are read from the provider's published pricing page at implementation time and recorded with their source. A rate that cannot be cited is not entered. The three entries in `MODEL_PRICING` today have no `source_url` and no date; whether their values are right is not the point — a reader cannot tell, and that is the defect. Entry 14 either re-sources them with provenance or drops them.

The failure policy is the important half, and `boss cost` already implements the first two bullets in its own vocabulary (`estimated_usd_partial`, `pricing_gaps`):

- A run whose `model_canonical` is NULL, or whose `(driver, model, date)` key has no rate row, is **`unpriceable`**. It is never priced at zero and never silently dropped from a sum.
- A figure computed over a set containing unpriceable runs reports **both** the priced subtotal **and** the unpriceable count, and is labelled partial. It never presents the priced subtotal alone.
- `cache_write_unsplit_tokens > 0` against a model with no `cache_write_unsplit_rate` makes that run unpriceable rather than guessing a TTL. This is where the design and `cost_pricing.rs` disagree most directly: its `estimate_usd` prices every cache write at one flat rate by explicit choice ("not worth modelling in an estimate a human is told to sanity-check"). The design's position is that the capture layer already records the split, the rates differ, and an estimate that discards known information is less trustworthy than one that refuses. Surfaced as an open question rather than silently overridden.
- Unpriceable causes are enumerated, not collapsed — `unknown_model`, `no_rate_epoch`, `unsplit_cache_write`, `capture_skipped`, `capture_unnormalised` — so a growing bucket is diagnosable. `capture_unnormalised` is new: it is the cause assigned to every pre-Layer-1 row of a gross-input driver, and it is what entry 24 uses to make today's `--usd` refuse those rows.

Rate-table changes are a human decision with money attached. The table is version-controlled and reviewed, not runtime-editable — see the open question on this, which this revision re-frames now that an in-code constant table exists.

### Layer 4 — Roll-up semantics

Three decisions the data cannot make for us. `boss cost task` has already made the first one implicitly and correctly.

**Attribution rule (non-negotiable foundation).** Every execution's spend attributes to its own `work_item_id`, always, with no cross-row re-attribution ever written to the database. Re-attribution exists only as a _view_. `cost_records_for_work_item` (`cost_report_db.rs:57`) selects by `we.work_item_id` and nothing else; that is this rule, and it should be kept.

Two views, always distinctly labelled:

- **`direct`** — the row's own executions only. **Board-level totals sum `direct`, and only `direct`.** This is what `boss cost task` computes today, unlabelled.
- **`inclusive`** — `direct` plus the `direct` of `parent_task_id` descendants (chains are one level deep, verified in July; not re-verified this pass). This is the honest answer to "what did this chore cost me, all in", and it is the right default for a _single row's_ detail view.

**Do revision children roll into the parent? Yes — in `inclusive` only, never in a board sum.** Revision children held ~20% of cache-read and ~21% of output relative to parent-kind rows at measurement time; ignoring them under-counts a chore by that much, and summing `inclusive` across a board double-counts exactly that slice. The recommendation is that a row's detail view leads with `inclusive` and shows `direct` beside it; a board or list view shows `direct` only.

**Do `pr_review` executions count? Yes as spend, but never blended into one number.** Executions bucket into three spend classes by `ExecutionKind` (`protocol/src/types/execution.rs:44`; eleven kinds, unchanged):

| Class            | Kinds                                                                                                                                             |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `implementation` | `task_implementation`, `chore_implementation`, `revision_implementation`, `investigation_implementation`, `ci_remediation`, `conflict_resolution` |
| `design`         | `project_design`, `product_design`                                                                                                                |
| `overhead`       | `pr_review`, `automation_triage`, `answer_agent`                                                                                                  |

Every figure reports the class split alongside the total. `boss cost window` already buckets `by_kind` at the eleven-kind grain; the three-class roll-up is a fold over that and should be added to the per-task report too, where today only the per-execution rows carry `kind`.

**How is low coverage surfaced?** Every figure carries a **coverage triple** — `executions_total`, `executions_with_usage`, `executions_priceable` — and a derived confidence. `CostMeasurement` already carries `runs_total`, `runs_measured`, `runs_unmeasured`, `runs_zero`; the delta is a `runs_priceable` count and a breakdown of `runs_unmeasured` by `cost_capture_status`, so that "not instrumented" and "capture skipped" stop sharing a number.

- **`none`** — `executions_priceable == 0`. The system shows **no figure**: an em dash plus the reason. It never shows `$0.00`. (`boss cost` gets this right today for the unmeasured case: "Total tokens: unknown".)
- **`partial`** — `0 < executions_priceable < executions_total`. The figure is shown, always with the ratio adjacent and always labelled as a floor.
- **`complete`** — every execution priced.

### Layer 5 — Read surface

**Revised against #2596.**

- **`boss cost task <selector>`** — exists. Gains: the coverage triple with its status breakdown, confidence, the three-class split, `direct`/`inclusive`, and a per-model breakdown from `work_run_model_usage`. `--json` exists.
- **`boss cost runs <selector>`** (new) — per-run drill-down, including `cost_capture_status` and unpriceable causes. This is the "why is this row partial" answer; `boss cost top` shows individual runs but only the expensive ones.
- **`boss cost rates`** (new) — the effective rate table with provenance and dates, so an operator can see what a figure was computed against. Today the only way to see the rates is to read `cost_pricing.rs`.
- **`boss cost window` / `boss cost top`** — exist. Gain the same coverage and class-split fields; `top`'s ranking key becomes the disjoint-class sum so drivers rank comparably.
- **`boss task show --json`** — gains a `cost` sub-object with the same shape as `cost task --json`. Still absent today; `work_runs` is not on that payload at all.
- **macOS app** — deferred. The CLI surface answers the operator question.

The original named these as `bossctl` verbs. That was wrong: `bossctl` is the coordinator-only binary and has no cost verb, correctly. The landed verbs are on `boss`, which is where an operator or worker asks the question, and the design follows.

### What the system shows where it cannot attribute

Stated explicitly, because "honest refusal" is a deliverable and not a disposition:

| Situation                                           | What is shown                                                                                                                                                    |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pre-instrumentation row (~4017 rows)                | `—` with `not instrumented — this work predates cost capture (2026-07-27) and cannot be recovered`. Permanent. (`boss cost` already renders these as "unknown".) |
| Capture skipped (`cost_capture_status != captured`) | `—` or `partial`, with the specific skip reason named. Never conflated with zero usage, and never conflated with pre-instrumentation.                            |
| Captured before Layer 1 on a gross-input driver     | `partial`, with `capture_unnormalised` named and the run counted as unpriceable. Tokens are still shown; dollars are not.                                        |
| Unpriceable model                                   | Priced subtotal plus `N runs unpriceable (unknown_model: <raw string>)`. The raw provider string is shown so the gap is actionable.                              |
| No rate epoch for the run's date                    | Same, with `no_rate_epoch` and the date.                                                                                                                         |
| Driver with no extractor (`driver_unsupported`)     | `—` with the driver named. This is how a fourth driver's runs look until it implements the capability.                                                           |
| Genuinely zero usage                                | `$0.00`, with confidence `complete`. This is the _only_ path to a displayed zero.                                                                                |

## Risks / open questions

- **The Codex containment relation is assumed, not proven.** `fresh_input = input − cached − cache_write` is the natural reading, but the only in-repo sample has a zero cache-write. Shipping the Codex extractor on an unconfirmed relation would re-create defect 2 in a new form. Mitigation: the investigation task is sequenced _before_ the extractor, and the extractor treats a negative result as a hard error rather than a clamp. The durable store lowers the cost of the investigation.
- **Reasoning-token inclusion is unresolved for Codex and Grok.** Unchanged. Sequenced as its own investigation.
- **The Grok usage record shape is sampled from `grok-4.5-build`; dispatch is now `grok-4.6`.** Field names may have changed. Entry 3 must re-sample before entry 6 reads them.
- **The Codex coverage figure (25.9%) and its containment-refusal hypothesis predate a rework of the whole Codex capture path.** The number may be wildly different now in either direction. Entry 10 must re-measure before diagnosing, and must do so with `cost_capture_status` available (entry 1) so that a skip is countable.
- **The hook-target resolver may split one execution's cost across two rows.** `resolve_run_id_for_execution_hooks` (`work/run_rows.rs:1360`; was `executions_runs.rs:2753`) orders by unfinished → non-failed → has-transcript → newest, and its doc comment now spells that out. If the agent-session run has `finished_at` set while a newer unfinished non-failed sibling exists, late hooks would land on the sibling. **Partial test coverage now exists** (`work/tests/t01.rs:1571`, `cost_snapshot_prefers_agent_session_over_prestart_failure_sibling`, and `:1667` for the path-bearing-failed case) pinning the failed-sibling and path-bearing orderings; the specific finished-versus-unfinished case is still unpinned. The proposed fix is unchanged: prefer the run whose `transcript_path` equals the advertised path before falling back to the existing order, plus the missing test.
- **`boss cost --usd` today can mis-price the moment a gross-input model enters `MODEL_PRICING`.** The exposure is latent (no such entry exists), but nothing prevents someone from adding a `gpt` line tomorrow, and the code's own doc comment invites exactly that ("a new dated model snapshot needs no code change"). Entry 24 closes this before Layer 1 lands, by making the estimator refuse rows whose driver is gross-input and whose capture predates normalisation.
- **`work_executions.driver` is write-once per execution and NULL before its migration.** A retry run under a different driver in the same execution would be mis-keyed. This pass did not establish whether the scheduler ever does that; if it can, `work_runs` needs its own driver column after all, sourced from the same spawn config. Flagged, not resolved.
- **Rate-table drift is a correctness risk with no automated detector.** Unchanged. The Grok `costUsdTicks` oracle detects this for one driver.
- **Whether the durable transcript store is ever reclaimed is unverified beyond "no code path found".** If an operator tool or a future sweep cleans `<state root>/executions/<run_id>/transcripts`, entry 25's window closes. Worth a one-line note in whichever sweep is added, and worth confirming before scheduling entry 25.
- **Coverage will remain low for a long time.** Unchanged, and now visible in `boss cost` output as large `unmeasured` counts. This is correct behaviour and should not be mistaken for the feature failing.
- **`cache_write_unsplit` may prove to be the common case for Codex.** Unchanged.
- **Per-model attribution assumes the provider names the model on the usage-bearing record.** Unchanged. For Codex the model arrives on a separate `turn_context` record. `work_executions.model` now gives a launched-model cross-check that did not exist before.

## Proposed implementation task breakdown

Dependency depths are marked. Entries at the same depth may run in parallel unless a file-overlap note says otherwise. Entries re-scoped in this revision say so.

---

**1. Persist `cost_capture_status` on `work_runs`; key runs to the launch-tuple driver**

_Re-scoped._ Add one column (`cost_capture_status`) via a new migration in the engine's migration chain, assert it in `schema_init`, and set it on the existing idempotent snapshot path in `work/run_rows.rs::set_run_cost_snapshot`. Values: `captured` / `skipped_containment_unresolved` / `skipped_no_transcript` / `no_usage_observed` / `driver_unsupported`, set from the branches in `app/worker_events.rs:520-560` that currently skip or fall through silently. The `driver` column the original proposed is **not** added: `work_executions.driver` (the launch tuple) is the driver key, with `driver_transcript::resolve_execution_driver_slug` as the fallback for NULL launch tuples. No behaviour change beyond recording; this makes the coverage question measurable and is what entry 10 needs.

- Effort: `small`
- Depends on: none
- Scope: in-scope

---

**2. Pin the hook-target run resolver's finished-versus-unfinished case with a test, and prefer transcript-path match**

_Re-scoped: two of the orderings are now pinned; one is not._ Add a test to `work/tests/t01.rs` alongside `cost_snapshot_prefers_agent_session_over_prestart_failure_sibling` covering a finished agent-session run alongside a newer unfinished non-failed sibling. If it confirms the split, prefer the run whose `transcript_path` equals the hook's advertised path in `resolve_run_id_for_execution_hooks` (`work/run_rows.rs:1360`) before the existing order. If refuted, keep the test as a regression pin.

- Effort: `small`
- Depends on: entry 1 (edits the same file; land after)
- Scope: in-scope

---

**3. Investigation: cross-driver token vocabulary reconciliation**

Study real Claude transcripts, Codex rollouts, and Grok `updates.jsonl` captures — the latter two now available under Boss's durable transcript store for post-2026-08-04 runs — to establish, per driver and with cited evidence: whether `input` is gross or net; whether cache-write tokens are contained in `input`; whether reasoning tokens are inside `output` or additive; whether Codex can switch models mid-session; and **whether `grok-4.6` still emits the `turn_completed.usage` shape sampled from `grok-4.5-build`**. Produce a written mapping table from each driver's native fields into the disjoint vocabulary, with the arithmetic identity that validates each. Deliverable is a document under `tools/boss/docs/investigations/`, not code.

- Effort: `medium`
- Depends on: none (may run in parallel with entry 1)
- Scope: in-scope

---

**4. Add a driver-owned cost-extraction capability to `AgentDriver`, with the Claude implementation**

Define the canonical usage record and a new capability on the `AgentDriver` trait (`engine/driver/src/lib.rs`) alongside the existing transcript capabilities. Implement it for Claude as a behaviour-preserving move of `CostAccumulator::ingest_assistant` (`run_cost.rs:100-153`), and rewire `run_cost.rs` to call the driver instead of matching on record shape itself. A driver that does not implement the capability yields `driver_unsupported`, not a silent fall-through. Existing `run_cost.rs` tests must pass unchanged.

- Effort: `medium`
- Depends on: entries 1, 3
- Scope: in-scope

---

**5. Codex cost extraction: gross-to-net normalisation**

Implement the extraction capability for Codex in `engine/driver/src/codex.rs`, mapping `event_msg/token_count`'s `total_token_usage` into the disjoint vocabulary using the containment relation established by entry 3. A negative `fresh_input` is a hard error that marks the run unpriceable, never a clamp to zero. Preserve the existing cumulative-versus-incremental handling.

- Effort: `small`
- Depends on: entry 4
- Scope: in-scope

---

**6. Grok cost extraction from `updates.jsonl` `turn_completed`**

Implement the extraction capability for Grok. The `turn_completed` record is already parsed and its `usage` object discarded at `engine/driver/src/grok/transcript.rs:181`; read it there. Map `inputTokens`/`cachedReadTokens` gross-to-net, and prefer the per-model `modelUsage` map over the flattened totals. Capture `costUsdTicks` into a column for later oracle validation; do not use it as the cost figure. Field names per entry 3's re-sample against `grok-4.6`.

- Effort: `small`
- Depends on: entries 3, 4
- Scope: in-scope
- Parallel with entry 5 — different driver files, no overlap.

---

**7. Per-model usage breakdown table**

Add the `work_run_model_usage` child table and write one row per `(run_id, driver, model_raw)` from the extraction results, replacing the run-level last-writer-wins model field as the pricing source. Retain `work_runs.model` as a denormalised display hint set to the model with the most output tokens, so `boss cost`'s existing `by_model` buckets and `ExecutionCostRow.models` keep working. This is the fix for defect 4.

- Effort: `medium`
- Depends on: entry 4
- Scope: in-scope
- Land **after** 5 and 6, forward-porting their extractors into the new write path preservingly.

---

**8. Driver-owned model-slug canonicalisation**

Add a canonicalisation function to each driver's `ModelMenu` mapping an observed model string to a canonical billable id (`opus` → `claude-opus-5`, `grok-4.6` → its canonical form, `gpt-5.6-sol` → its canonical form), and populate `model_canonical` from it. Unrecognised strings leave `model_canonical` NULL and retain `model_raw` verbatim. Sentinel non-models (`<synthetic>`) canonicalise to NULL by design. This replaces `cost_pricing::price_for_model`'s substring match, which is removed in entry 13.

- Effort: `small`
- Depends on: entry 7
- Scope: in-scope

---

**9. Claude subagent transcript enumeration**

When the driver declares its transcripts carry sidechains, enumerate the sibling `subagents/` directory alongside the advertised parent transcript path and fold those files into the same accumulator. Derive the paths from the parent path rather than adding a `SubagentStop` hook to `CLAUDE_HOOK_EVENTS`. Rely on the existing `message.id` dedup. Enumeration failure sets `cost_capture_status = partial_subagents_unavailable`. Recovers the measured ~25.4% of fresh input and ~12.4% of cache-creation currently uncounted.

- Effort: `medium`
- Depends on: entry 7
- Scope: in-scope

---

**10. Re-measure and close the Codex capture-coverage gap**

_Re-scoped: measure before diagnosing._ Using `cost_capture_status` from entry 1, measure Codex coverage on runs created after #2578/#2662/#2680 (all landed by 2026-08-08), and only then confirm or refute the containment-refusal suspicion at `app/worker_events.rs:520`. The 25.9% figure is from before those changes and must not be assumed. Fix whatever the root cause is. Degrading a contained driver to an unrestricted tail is **not** an acceptable fix.

- Effort: `medium`
- Depends on: entries 1, 5
- Scope: in-scope

---

**11. Reasoning-token accounting for Codex and Grok**

Implement whatever entry 3 establishes: if reasoning tokens are additive, populate `reasoning_billed_separately` and give the rate table a matching class; if contained, add an assertion and a test documenting the containment. Either way the outcome is pinned by a test rather than an assumption.

- Effort: `small`
- Depends on: entries 3, 5, 6
- Scope: in-scope

---

**12. Golden-corpus conformance sweep for cost extraction**

Extend the existing driver conformance goldens (`engine/core/src/conformance/`) with captured usage-bearing records from all three drivers, asserting each extractor produces the expected disjoint vocabulary and that the class sums reconcile against each provider's own reported totals.

- Effort: `medium`
- Depends on: entries 5, 6, 9, 11
- Scope: in-scope

---

**13. Rate table: schema, loader, and effective-date resolution, replacing `cost_pricing.rs`**

_Re-scoped from "introduce" to "replace"._ Introduce a small crate (per the repo's prefer-crates-over-modules convention) owning the rate table: schema keyed on `(driver, model_canonical, effective_from, effective_to)` with one rate per disjoint token class, the loader, provenance fields (`source_url`, `source_retrieved_at`), and date resolution against a run's `started_at`. Delete `engine/core/src/cost_pricing.rs` and its substring matcher; `cost_report.rs`'s `Accumulator::add` calls the new crate. Ships with an **empty table** — populating it is entry 14 — so between 13 and 14 every run is `unpriceable (no_rate_epoch)`, which is the honest state.

- Effort: `medium`
- Depends on: entry 8
- Scope: in-scope

---

**14. Populate the rate table from authoritative published pricing**

Data-only change. Read each provider's published pricing page at implementation time and enter one row per `(driver, model, rate epoch)` with `source_url` and `source_retrieved_at` recorded. The three families currently in `MODEL_PRICING` are re-sourced, not copied: a value that cannot be cited to a retrieved source is not entered. Include historical epochs back to 2026-07-27 where the provider publishes them.

- Effort: `small`
- Depends on: entry 13
- Scope: in-scope

---

**15. Pricing engine: priced usage and unpriceable classification**

Extend `cost_report.rs`'s `Accumulator` to price over `work_run_model_usage` joined to the rate table on `(driver, model_canonical, run date)`, classifying every unpriceable run by enumerated cause — `unknown_model`, `no_rate_epoch`, `unsplit_cache_write`, `capture_skipped`, `capture_unnormalised`. `estimated_usd_partial` and `pricing_gaps` already exist; this gives them causes.

- Effort: `medium`
- Depends on: entries 13, 7
- Scope: in-scope

---

**16. Roll-up query layer: `inclusive` view, spend classes, coverage triple**

Extend `cost_report.rs` / `cost_report_db.rs`: label the existing per-task aggregation as `direct`; add the `inclusive` view over `parent_task_id`; fold `by_kind` into the three-way `implementation`/`design`/`overhead` split and add it to the per-task report; add `runs_priceable` and a `runs_unmeasured` breakdown by `cost_capture_status` to `CostMeasurement`; derive `none`/`partial`/`complete`. Enforce as an invariant that board-level totals sum `direct` only.

- Effort: `medium`
- Depends on: entry 15
- Scope: in-scope

---

**17. Protocol: extend the cost types on the wire**

_Re-scoped: extend, not add._ Extend `protocol/src/types/cost_report.rs` with the coverage breakdown, confidence, class split, per-model breakdown, and unpriceable causes; add a `GetWorkItemCostRuns` request for entry 18's drill-down and a rate-table request for `cost rates`. Additive; existing fields keep their meaning.

- Effort: `small`
- Depends on: entry 16
- Scope: in-scope

---

**18. `boss cost runs` and `boss cost rates`; extend `task`, `window`, `top`**

_Re-scoped._ Add `Runs` and `Rates` to `CostCommand` (`cli/src/commands.rs:2899`) with handlers in `cli/src/cost_cmds.rs`; extend the three existing verbs' output with the new fields. Keep the presentation invariants `cost_cmds.rs` already honours (no `$0.00` for unmeasured; "ESTIMATE" on every USD figure) and add the new ones (`partial` always shows its ratio; `none` shows a reason).

- Effort: `medium`
- Depends on: entry 17
- Scope: in-scope

---

**19. `boss task show --json` cost block**

Add a `cost` sub-object to the task-show payload with the same shape as `cost task --json`. `work_runs` is absent from that payload entirely today.

- Effort: `small`
- Depends on: entry 17
- Scope: in-scope
- **File overlap with entry 18** in `cli/src/commands.rs` and the shared output helpers. Land entry 18 first.

---

**20. Grok `costUsdTicks` oracle validation sweep**

Diff Boss's computed cost against Grok's provider-reported `costUsdTicks` across every Grok run carrying both, and report systematic divergence. The only end-to-end accuracy check available anywhere in this system.

- Effort: `small`
- Depends on: entries 6, 14, 15
- Scope: in-scope

---

**21. macOS app cost surface**

Surface per-work-item cost, coverage, and confidence in the Boss app, honouring the same presentation invariants as the CLI.

- Effort: `medium`
- Depends on: entry 17
- Scope: deferred (future / not a v1 blocker)

---

**22. Rate-staleness detection for Claude and Codex**

Detect that a provider changed a published rate while the table still holds the old one. Minimum viable answer: a staleness warning driven off `source_retrieved_at`.

- Effort: `small`
- Depends on: entry 14
- Scope: deferred (future / not a v1 blocker)

---

**23. Backfill of pre-instrumentation and Claude runs — permanently rejected, do not schedule**

_Narrowed in this revision to what is actually impossible._ The ~4017 pre-instrumentation `in_review`/`done` rows **cannot** be backfilled: every one was a Claude run, Boss stores only a pointer into `~/.claude/projects`, and Claude Code's own cleanup has reclaimed the files. The same applies to every Claude run whose transcript has aged out, including 313 of the 331 runs whose subagent transcripts were already gone at measurement. **This entry must never be materialised into a task.** The correct handling is entry 16's confidence `none` with a "not instrumented" reason. Codex and Grok runs after 2026-08-04 are **not** covered by this rejection; see entry 25.

- Effort: `trivial`
- Depends on: none
- Scope: deferred (future / not a v1 blocker) — recorded as permanently rejected, not as pending work.

---

**24. Make `boss cost --usd` refuse rows it cannot price correctly (new)**

Before Layer 1 lands, make the existing estimator honest about the gross/net collision: a run whose execution's `work_executions.driver` (or resolved fallback) is a gross-input driver (`codex`, `grok`) and whose capture predates normalisation is classified `capture_unnormalised` and excluded from the USD figure with its own line in `pricing_gaps`, exactly as an unknown model is today. Also drop the flat cache-write pricing in favour of refusing when the split is unknown, or record the decision to keep it as an explicit open-question outcome. Small, and it removes the latent mis-pricing before anyone adds a `gpt` family to the table.

- Effort: `small`
- Depends on: none (reads `work_executions.driver`, which exists)
- Scope: in-scope

---

**25. Bounded re-extraction of durable Codex and Grok transcripts (new)**

Once the Codex and Grok extractors exist, walk `<state root>/executions/<run_id>/transcripts/<driver>/sessions` for every run recorded since 2026-08-04 whose `work_run_model_usage` is empty, re-run extraction, and write the per-model rows with `cost_capture_status = captured`. This is the only backfill the data supports and it is bounded by what the durable store holds. Confirm first that nothing reclaims that directory (see the open question); if something does, scope this to whatever survives and say so in the output.

- Effort: `medium`
- Depends on: entries 5, 6, 7
- Scope: in-scope

---

### Parallelism summary

| Depth | Entries      | Notes                                                                                           |
| ----- | ------------ | ----------------------------------------------------------------------------------------------- |
| 0     | 1, 3, 24     | Fully parallel — engine schema, investigation doc, and a small guard on the existing estimator. |
| 1     | 2, 4         | 2 depends on 1; 4 depends on 1 and 3.                                                           |
| 2     | 5, 6         | Fully parallel — separate driver files.                                                         |
| 3     | 7            | Serialised after 5 and 6: shares `run_cost.rs` persistence path.                                |
| 4     | 8, 9, 10, 25 | Parallel; 10 also needs 5; 25 needs 5, 6, 7. 8 and 9 touch different layers.                    |
| 5     | 11, 13       | Parallel.                                                                                       |
| 6     | 12, 14       | Parallel — 12 is a test sweep, 14 is data.                                                      |
| 7     | 15           | —                                                                                               |
| 8     | 16, 20       | Parallel — 20 is validation, 16 is the query layer.                                             |
| 9     | 17, 22       | Parallel.                                                                                       |
| 10    | 18, 21       | 21 deferred.                                                                                    |
| 11    | 19           | Serialised after 18: CLI file overlap.                                                          |
