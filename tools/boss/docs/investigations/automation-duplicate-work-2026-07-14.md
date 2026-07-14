# Reducing duplicate work filed by independent automation runs

**Date:** 2026-07-14
**Trigger:** operator-observed incident, 2026-07-13: two automation runs independently filed near-identical dedup chores ~29 minutes apart (T2572 → PR [#1963], T2574 → PR [#1966]), both migrating `engine/core/src/runner.rs`'s `extract_pr_number` onto `boss_github::pr_url::pr_number_from_url`.
**Status:** investigation complete; recommended design at the end, phased and ready to convert into implementation tasks.
**Out of scope:** implementing the fix; human-filed duplicates (different problem, though layers 2–3 below would partially catch those too).

[#1963]: https://github.com/spinyfin/mono/pull/1963
[#1966]: https://github.com/spinyfin/mono/pull/1966

## TL;DR

There is **no duplicate-work gate anywhere in the automation create path**. The only dedup in the engine is a 60-second exact-name guard (`insert_helpers.rs::check_recent_duplicate`), and automation-sourced creates **explicitly bypass it** (`force_duplicate(true)` in `WorkDb::create_automation_task`, `work/automations.rs:836`). Automation triage keys purely on repo state — which only changes when a PR _merges_ — while the observed file→merge latency was ~8 hours and the fire cadence ~30 minutes. Any automation (or pair of automations with similar instructions) whose cadence is shorter than the merge latency of its own output will re-derive the same findings and file guaranteed-conflict duplicates.

The 2026-07-13 incident was not one duplicate pair but **two**, produced in the same double-fire window: T2572/T2574 (dedup chores, identical single-file migration) _and_ PRs [#1962]/[#1964] (pr_flow behavior tests, identical two-file set). One of those four PRs ([#1964]) was still open and unreconciled at the time of writing.

**Recommendation:** a three-layer design — (0) inject work-system context into the triage prompt so the model itself declines duplicates, (1) a structural pre-file gate at `create_automation_task` keyed on declared target files, and (2) a post-hoc file-set overlap detector in `merge_poller`. Every suppression is recorded as an `automation_runs` outcome and surfaced as an attention item — nothing is silently dropped.

[#1962]: https://github.com/spinyfin/mono/pull/1962
[#1964]: https://github.com/spinyfin/mono/pull/1964

---

## 1. Case study: the 2026-07-13 double collision

### 1.1 Timeline

All times UTC. Execution ids embed a nanosecond creation timestamp (first hex segment), which gives us dispatch traces directly from the PR branch names without needing coordinator DB access.

| Time             | Event                                                                                                           | Evidence                                  |
| ---------------- | --------------------------------------------------------------------------------------------------------------- | ----------------------------------------- |
| Jul 13 21:31:45  | Test-sweep task A filed                                                                                         | `exec_18c1f6d1324c3d10` (branch of #1962) |
| Jul 13 21:45:52  | **PR #1962 opened** — behavior tests for pr_flow state transitions; files: `work/tests.rs`, `work/tests/t21.rs` | `gh pr view 1962`                         |
| Jul 13 21:47:02  | **T2572 filed** (dedup `extract_pr_number` → `pr_number_from_url`)                                              | epoch 1783979222; `exec_18c1f7a6924e9390` |
| Jul 13 21:52:33  | **PR #1963 opened** — files: `engine/core/src/runner.rs` only                                                   | `gh pr view 1963`                         |
| Jul 13 22:01:14  | Test-sweep task B filed                                                                                         | `exec_18c1f86d1fe35d28` (branch of #1964) |
| Jul 13 22:13:46  | **PR #1964 opened** — _identical two-file set as #1962_                                                         | `gh pr view 1964`                         |
| Jul 13 22:16:23  | **T2574 filed** — same migration as T2572                                                                       | epoch 1783980983; `exec_18c1f9409a9281d8` |
| Jul 13 22:21:17  | **PR #1966 opened** — _identical single file, identical migration as #1963_                                     | `gh pr view 1966`                         |
| Jul 14 06:00:28  | #1962 and #1963 merged                                                                                          | `mergedAt`                                |
| Jul 14 06:02:13  | #1966 closed by operator as duplicate; this investigation (T2581) filed                                         | close comment on #1966                    |
| Jul 14 (ongoing) | **#1964 still open** — live unreconciled collision                                                              | `gh pr view 1964`                         |

### 1.2 Window structure: two interleaved fire streams

The four task creations sit at 21:31:45, 21:47:02, 22:01:14, 22:16:23 — inter-arrival gaps of 15m17s, 14m12s, 15m09s. Grouped by topic they form two streams:

- **Test stream:** fires at 21:31:45 and 22:01:14 — gap **29m29s** → PRs #1962/#1964, identical file sets.
- **Dedup stream:** fires at 21:47:02 and 22:16:23 — gap **29m21s** → PRs #1963/#1966, identical file + identical migration.

So the T2572→T2574 "~29 minutes" was not a one-off: two topic streams each re-fired on a ~29.5-minute period, phase-offset by ~15 minutes, and _each_ stream collided with itself. Both second fires happened while the first fire's PR was already open (PR #1963 had been open 24 minutes when T2574 was filed) but unmerged — so the second triage run scanned an unchanged `main` and deterministically re-derived the same finding.

### 1.3 Same automation re-firing, or two different automations?

The code makes "one automation, default config" **impossible** for either pair:

- The open-task cap counts `in_review` as open (`count_open_tasks_for_automation`, `work/automations.rs:251-262`: `status IN ('todo','ready','active','in_review','blocked')`).
- It is enforced at fire time (`automation_scheduler.rs:370`, outcome `suppressed_at_limit`) _and_ transactionally re-checked at create (`work/automations.rs:809-823`).
- T2572 was `in_review` (PR #1963 open, merged only at 06:00 next day) when T2574 was created at 22:16. Same for the test pair.

Therefore each colliding pair came from **two different automations with overlapping standing instructions** (e.g. two dedup-flavored sweeps on the same product), or from automation(s) configured with `open_task_limit ≥ 2`. Either way the demonstrated gap is exactly the work item's title: _independent_ automation runs converging — the per-automation cap, the only volume control in the system, is structurally blind to this.

The discriminating evidence (the `automation_id` on the four `automation_runs` rows and their standing instructions) lives in the coordinator DB, which workers must not read; the coordinator should attach those rows to T2581 to confirm. Nothing in the recommended design depends on which hypothesis holds — both are un-gated today.

### 1.4 This target family is a recurring convergence point

The "consolidate PR-URL parsing" target has been hit by automation-filed PRs **five times in ten days**, all on `boss/exec_*` branches:

| PR                                                  | Created | Files                                                              |
| --------------------------------------------------- | ------- | ------------------------------------------------------------------ |
| [#1797](https://github.com/spinyfin/mono/pull/1797) | Jul 4   | `cli/src/main.rs`, `work/chain_helpers.rs`                         |
| [#1820](https://github.com/spinyfin/mono/pull/1820) | Jul 6   | `merge_poller.rs`, `work/chain_helpers.rs`, `github/src/pr_url.rs` |
| [#1833](https://github.com/spinyfin/mono/pull/1833) | Jul 8   | `engine/core/src/runner.rs`                                        |
| [#1963]                                             | Jul 13  | `engine/core/src/runner.rs`                                        |
| [#1966]                                             | Jul 13  | `engine/core/src/runner.rs` (duplicate of #1963)                   |

#1797–#1963 each removed _different residual_ call sites, so they are legitimate incremental work rather than duplicates — but they show the softer failure mode: sweeps keep re-discovering the same hotspot because nothing records "this territory was swept on date X." T2572's brief still named `chain_helpers.rs` even though that half had already landed on `main` days earlier (confirmed in #1963's body: the worker found it already done and no-op'd that half). Stale briefs waste worker time re-verifying landed work even when they don't collide.

### 1.5 Cost accounting for the incident

- 2 duplicate worker dispatches (T2574 → #1966; test task B → #1964), each a full lease-work-test-PR cycle (`bazel test //tools/boss/engine/...`, 21 targets, per both PR bodies).
- 2 extra review pipelines.
- Operator attention to spot, diff-compare, and close #1966 (~8 hours after filing); #1964 still needs the same triage.
- Guaranteed merge conflict or no-op for whichever of an identical pair merges second.

Incidence scales with automation count × cadence ÷ merge latency. All three are trending the wrong way: more automations, faster cadence, and merge latency dominated by human review availability (the case-study PRs waited ~8 hours overnight).

---

## 2. The collision surface

### 2.1 Which automations file work items

Automations are **user-configured rows** (`automations` table, DDL at `work/migrations_b.rs:1175-1224`), not a hardcoded catalog: each carries a free-text `standing_instruction` (the design doc's own examples: "Fix clippy warnings", "Look for duplicated code and extract a helper", dependency bumps — `tools/boss/docs/designs/maintenance-tasks.md`), a cron `trigger_config` (`automation_schedule.rs`), an `open_task_limit` (default 1), and a `product_id`. The observed sweep taxonomy on Jul 13 (~30 merged automation PRs in one day) is dominated by dedup/refactor chores and test-coverage chores.

Review followups are a **separate mechanism** (provenance via `origin_task_short_id`/`origin_pr_number`, `work/migrations_b.rs:1580-1596`), not automation triage; they inherit narrow scope from a specific PR and are less collision-prone, but the post-hoc detector (layer 2) covers them too since it keys on PR file sets regardless of source.

### 2.2 The create path, end to end

1. `automation_scheduler::evaluate_one` (`automation_scheduler.rs:257`) decides a fire is due → `EngineTriageDispatcher::fire` (`automation_triage.rs:358`) creates a `work_executions` row of kind `automation_triage` and records an `automation_runs` row keyed `(automation_id, scheduled_for)`.
2. The triage worker gets a rendered preamble (`render_triage_preamble`, `automation_triage.rs:62-138`) telling it to derive "a **single, concrete, actionable** task … **right now**" from the standing instruction by examining the repo, then emit exactly one marker: `automation: task T42` or `automation: skip — <reason>`.
3. To file, the worker calls `boss task create --automation <id>` → `handle_create_automation_task` (`app/automations.rs:367-431`) → `WorkDb::create_automation_task` (`work/automations.rs:804-846`): transactional cap re-check, then `insert_chore_in_tx(...)` with `force_duplicate(true)`, then `source_automation_id` stamped.
4. The chore dispatches through the normal coordinator path to the automation pool; the worker opens a PR; `merge_poller` walks it to `done` at merge.

### 2.3 What runs key on, and why they converge

Triage is **stateless with respect to the work system**. Its only inputs are (a) the standing instruction and (b) repo state at fire time. It has no visibility into open tasks, sibling automations, open PRs, or recently merged automation output — the preamble contains no instruction to check for any of these (verified by inspection of `automation_triage.rs`). Two runs with similar instructions scanning the same commit are running a near-deterministic function on identical inputs; converging on the same "most obvious" target is the expected outcome, not bad luck. The convergence window is **not** the 29 minutes between fires — it is the _file→merge latency_ of the first finding (~8 hours here), during which `main` still exhibits the smell and every scan re-derives it.

### 2.4 Existing guards, and why each one misses

| Guard                                   | Where                                                                         | Why it misses this                                                                                                                                                                       |
| --------------------------------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 60s exact-name duplicate guard          | `insert_helpers.rs:94-126`, `DUPLICATE_GUARD_WINDOW_SECS = 60` (`work.rs:90`) | Same-product + exact trimmed-name + 60s only; and automation creates bypass it via `force_duplicate(true)`                                                                               |
| Open-task cap                           | `automation_scheduler.rs:370`; `work/automations.rs:809-823`                  | Per-automation volume control; blind across automations; configurable upward                                                                                                             |
| Chain hold (single-writer per PR chain) | `coordinator.rs:1890` (`ChainHold`), added after incident-001-pr-fan-out      | Keys on shared PR-chain membership; independent tasks have no chain edge                                                                                                                 |
| PR-review dedup                         | `work/dispatch.rs:99`                                                         | Per-PR head-sha; dedups review executions, not work items                                                                                                                                |
| Marker-recovery                         | `completion.rs:2372-2392`                                                     | Prevents _retry_ over-production within a single automation's runs only                                                                                                                  |
| merge_poller conflict handling          | `merge_poller.rs`                                                             | Only reads GitHub's `mergeable`/`mergeStateStatus`; its `gh` projection (`merge_poller.rs:573`, 634-636) does not request `files`, so it cannot see two open PRs touching the same paths |

A dedup sweep is already an acknowledged gap: `app.rs:1007` — `// TODO(@brianduff,2026-12-31): spawn one-shot dedup sweep background task.`

### 2.5 What keys would have caught the incident

Lexical name similarity is **weak** here: the colliding pair's titles ("route runner PR-number parsing through boss_github helper" vs "dedup runner extract_pr_number onto boss_github::pr_url") share tokens but wouldn't clear a high-precision string threshold. Structural keys are **decisive**: both briefs/PRs name the same file (`engine/core/src/runner.rs`) and the same symbols (`extract_pr_number`, `pr_number_from_url`); both PR file sets are identical. The same holds for the #1962/#1964 pair (identical two-file set). File paths and symbols named in the brief are the high-precision signal; name/description similarity is a tie-breaker, not a gate.

---

## 3. Option families

### a. Pre-file dedup gate (structural, at create)

Before an automation-sourced create commits, compare the candidate against (i) open automation-sourced task rows and (ii) file sets of open `boss/*` PRs. Matching keys, in decreasing precision: declared target file paths; symbols named in the brief; lexical similarity on name/description; (later, if needed) embedding similarity.

- **For:** kills the demonstrated failure mode at the cheapest point (one SQL query + one comparison, inside the existing `create_automation_task` transaction). Both incident pairs match on exact-file-set equality — the crudest possible key.
- **Against:** needs the triage worker to _declare_ target files (new `--target-file` args on `boss task create`; briefs already name them in prose, so this is formalizing existing behavior, but it is a prompt-contract change and the worker can under- or mis-declare). False positives: two genuinely distinct chores touching the same file (e.g. #1945 vs #1955 both touched `string_clip.rs` on Jul 13 doing _different, sequential_ work). Mitigation: gate on high-precision predicates only (identical/subset file sets **and** name-token overlap above a threshold), and make a hit _surface_ rather than hard-fail (§4).
- **Verdict: adopt** (layer 1). High precision on the observed collisions, tiny cost, right place.

### b. Claim/lease on targets

Automations declare target files/symbols; the engine holds claims so a second run skips claimed targets. Prior art: cube workspace leases (`tools/cube/src/store.rs`, TTL + sweep; engine-side `cube_lease_heartbeat.rs`, `pool_claim_sweep.rs`).

- **For:** naturally covers the create→merge window, which is the actual exposure window.
- **Against:** mostly collapses into option (a) once you notice the claim's correct lifetime is "until the owning task reaches a terminal state" — which is exactly what an open task row with declared targets already expresses. A separate TTL'd claim store adds expiry tuning, heartbeat plumbing, and a reclaim sweep for marginal benefit over querying open rows. Speculative claims (triage claims files it never ends up touching) block siblings on nothing.
- **Verdict: fold into (a).** Represent the "claim" as declared-targets-on-the-open-row; no separate lease machinery. Revisit only if we later want claims _before_ a row exists (e.g. at triage start), which the incident does not require.

### c. Sweep-scope partitioning (territory/cursor)

Make runs idempotent over a durable cursor: "this sweep owns these crates/dirs this window."

- **For:** would also fix the _serial re-discovery_ waste (§1.4) — a swept-territory record stops re-scanning ground swept yesterday.
- **Against:** does not help the demonstrated case unless territories are assigned _across_ automations, which requires central assignment and fights the free-text standing-instruction model (an instruction like "fix clippy warnings" has no natural partition). Meaningful schema + scheduler work; the payoff overlaps heavily with (a) + (e).
- **Verdict: defer.** Reconsider if telemetry shows serial re-discovery remains expensive after layers 0–2; a lighter variant (inject "recently swept/merged targets" into the prompt) ships inside layer 0.

### d. Post-hoc reconciliation (overlap detector)

A sweep that detects open sibling PRs with overlapping file sets and parks/flags the younger row for coordinator triage.

- **For:** catches everything the pre-gate misses (mis-declared targets, human/automation collisions, collisions where the second create predates the first PR). `merge_poller` already polls every `in_review` PR on a cadence; adding `files` to its `gh` projection and a pairwise file-set comparison over open automation PRs is incremental, not new infrastructure. It converts an 8-hour operator-spotted collision into a minutes-scale attention item — #1964 would have been flagged at ~22:14 on Jul 13 instead of remaining open past the next morning.
- **Against:** fires _after_ the duplicate worker has already run (the dispatch cost is spent); needs care not to spam on legitimate overlapping-but-sequenced work (stacked PRs, planner `merge_order_hints`). Mitigation: only compare automation-sourced PRs pairwise, require high overlap (Jaccard ≥ threshold or identical sets), exclude same-chain PRs (chain metadata already exists).
- **Verdict: adopt** (layer 2). It is also the _measurement instrument_: its hit counter is the ground truth that proves layers 0–1 are working (hits should trend to zero).

### e. Automation-context injection

Feed each triage run the list of recently filed automation rows (name + declared targets + status) and open `boss/*` PR file sets, plus a prompt instruction: "if your candidate overlaps one of these, emit `automation: skip — duplicate of Txxxx` instead."

- **For:** cheapest by far — a change to `render_triage_preamble` and the dispatcher that renders it; no schema, no new query path beyond what the engine already stores. In the case study it would very probably have prevented both duplicates: at 22:16 the context block would have contained "T2572 (in*review, PR #1963): dedup runner `extract_pr_number` → files: `engine/core/src/runner.rs`" — an unmissable match for a model already instructed to skip. Also mitigates serial re-discovery by listing recently \_merged* automation PRs.
- **Against:** least reliable — it is a probabilistic guard with no hard guarantee; the model can ignore context, and reliability degrades as the context list grows. **Quantification:** treat it as risk reduction, not elimination; do not count it toward correctness. Its expected value is high precisely because the observed collisions are blatant (identical files, near-identical descriptions), the easiest class for a model to decline. Measure via skip markers whose reason cites a duplicate, cross-checked against layer-2 residuals.
- **Verdict: adopt** (layer 0, ship first). It buys immediate risk reduction while layers 1–2 are built, and afterwards remains useful for the _semantic_ near-duplicates a structural gate can't see.

---

## 4. Recommended design: three layers, no silent loss

### Layer 0 — context injection (triage prompt)

**Lives in:** `automation_triage.rs::render_triage_preamble` + `EngineTriageDispatcher::fire` (which gathers the context at fire time).

At fire time the dispatcher queries: open automation-sourced tasks for the _product_ (all automations, not just the firing one — the cross-automation blindness is the bug) with name, status, PR URL, declared targets; plus automation PRs merged in the last N days (title + files). Rendered as a "Recently filed / in-flight automation work" block with the instruction to emit `automation: skip — duplicate of <ref>` on overlap. Skip reasons are already persisted verbatim on the `automation_runs` row (`finalize_automation_triage_run`), so this layer needs no new telemetry schema.

### Layer 1 — structural pre-file gate (create path)

**Lives in:** `WorkDb::create_automation_task` (`work/automations.rs`), inside the existing `Immediate` transaction, next to the cap re-check.

1. **Target declaration:** `boss task create --automation` grows repeatable `--target-file <path>` (and optionally `--target-symbol <name>`) args; the triage preamble requires declaring the files the task is expected to touch. Stored in a small `task_targets` side table (`task_id`, `kind`, `value`) — a side table rather than a column so the post-hoc layer and future human-task use can share it.
2. **Gate predicate (high precision only):** suppress when the candidate's declared file set is equal to, or a subset of, the declared/actual file set of an open automation-sourced row or open automation PR **and** name/description token overlap clears a threshold. Anything weaker (same file, different work — the #1945/#1955 pattern) passes through untouched; the post-hoc layer covers the gray zone.
3. **Failure mode — surfacing, not dropping:** on a gate hit, `create_automation_task` returns a structured "duplicate-suspect of Txxxx" error to the triage worker, which emits `automation: skip — duplicate of Txxxx`. The run is finalized with a **new `automation_runs` outcome `suppressed_duplicate`** (alongside the existing `suppressed_at_limit`, `types.rs:399-405`) carrying the blocking row's id, **and an attention item is filed** linking suppressed-candidate → blocking row. The operator sees every suppression, can ack it, or can override (a coordinator-side re-file with `force_duplicate` semantics) if the gate was wrong. A suppressed create is therefore never silent: worst case for a false positive is one operator ack, versus today's worst case of a dispatched duplicate worker + review pipeline + merge conflict.

### Layer 2 — post-hoc overlap detector (merge_poller)

**Lives in:** `merge_poller.rs`.

Add `files` to the PR projection it already fetches, persist the file set per in-review row (also backfilling `task_targets` with _actual_ touched files, improving layer 1's data), and each cycle compare open automation-sourced PRs pairwise (excluding same-chain pairs). On overlap ≥ threshold: park the **younger** row (`blocked: duplicate_suspect` or equivalent hold) and file an attention item naming both PRs and the shared files for coordinator triage. Increment a **duplicate-collision counter**.

### Telemetry — proving it works

- `suppressed_duplicate` outcomes per automation per window (layer 1 hits).
- Skip markers citing duplicates (layer 0 hits).
- The layer-2 collision counter is the **residual** — collisions that reached a dispatched worker despite layers 0–1. Success criterion: layer-2 residual trends to ~0 while layer-0/1 counters show the gates are actually engaging (all three at zero would mean the collisions stopped arriving, or the gates are dead — distinguishable because layer 0/1 hits are expected while automations overlap).
- All three are answerable from `automation_runs` + attention rows; no new metrics infrastructure required.

### Phasing

| Phase        | Contents                                                                                                | Size                                                           |
| ------------ | ------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- |
| 0            | Context injection into triage preamble + skip-reason convention                                         | small — prompt + dispatcher query, no schema                   |
| 1            | `task_targets` table, `--target-file` on create, gate + `suppressed_duplicate` outcome + attention item | medium — schema migration, create-path change, prompt contract |
| 2            | merge_poller `files` projection + pairwise overlap + park-younger + collision counter                   | medium — poller change, one new blocked-reason                 |
| 3 (deferred) | Embedding similarity in the gate; territory partitioning; claims-before-create                          | only if layer-2 residuals justify it                           |

Phases 0–2 are independent enough to land as separate PRs in that order; each is valuable alone.

---

## 5. Open questions for the coordinator

1. **Confirm the automation identities** behind the four Jul 13 fires (the `automation_runs` rows for T2572/T2574 and the two test tasks): two automations with overlapping instructions, or raised `open_task_limit`s? Workers cannot read the coordinator DB; §1.3 shows default-config-single-automation is impossible, but the concrete configuration should be attached to T2581 for the record. If it _is_ two automations with near-identical standing instructions, the cheapest immediate mitigation is editing/merging the instructions — worth doing regardless of the engine work.
2. **Threshold choices** (file-set Jaccard, name-token overlap) should start strict (identical/subset sets only) and be tuned from layer-2 residual data, not guessed up front.
3. Whether layer 2 should also compare automation PRs against **human** PRs (out of scope for the duplicate problem, but the same overlap signal feeds the existing merge-conflict-reduction design, `docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md`).

## Follow-up implementation tasks (to file separately — not part of this PR)

1. **Phase 0:** inject in-flight/recent automation work into the triage preamble; teach the prompt the `skip — duplicate of <ref>` convention.
2. **Phase 1:** `task_targets` schema + `--target-file` declaration + structural pre-file gate + `suppressed_duplicate` outcome + suppression attention item.
3. **Phase 2:** merge_poller file-set overlap detector + park-younger + duplicate-collision counter.
