# Should the PR-review pool keep hardcoding the expensive model tier?

- **Date:** 2026-07-30
- **Kind:** investigation / writeup — no engine code changed, and none proposed inside this document's scope
- **Code sha:** every `file:line` citation is against `main` @ `492aa9cc9d1f5e69e3d6c4cd6009bb1989fe5eca`
- **Question:** the review pool picks its model independently of the reviewed row's reasoning mode and always picks the expensive tier. Was that decided, and should it stay?
- **Related:** [`designs/automated-reviewer-pass-on-every-agent-authored-pr.md`](../designs/automated-reviewer-pass-on-every-agent-authored-pr.md), [`designs/effort-and-model-estimation.md`](../designs/effort-and-model-estimation.md), [`forensic-surfaces.md`](../forensic-surfaces.md)

## Verdict

**Keep Opus for review, for now — but treat that as a re-affirmation with an expiry date, not a default.** The hardcoding was a deliberate design decision with a written rationale, so the "nobody decided this" premise is wrong. What has quietly stopped being true is the _cost bound_ that decision shipped with: the design paired always-Opus with a deliberately small pool to cap concurrent spend, and that pool has since been raised from 2 to 8 with the bound explicitly written off.

Do not move review to Sonnet on the current evidence. Not because Sonnet would be worse — that is unknown — but because **nothing in the system can currently tell a good review from a bad one**, so the change would be unmeasurable in either direction, and the failure mode it risks (a missed regression that reaches `main`) is the one the whole subsystem exists to prevent.

Two changes pay off before the tier question and are independent of it: making the severity gate consult the `confidence` field it already collects and ignores, and persisting review outcomes so the tier question becomes answerable at all. Both are cheaper than a tier change and neither is a one-way door.

## What was verified, and what was not

Everything in the _Where the decision lives_, _What review quality actually depends on_, and _What the data can and cannot answer_ sections is verified by reading code and design docs at the sha above. File and line citations are exact.

Not verified, and deliberately not relied on:

- **The token-count ratio** in the task description (cheaper tier ≈ 60% of current review cost). That is arithmetic on token counts, not a quality measurement, and it is not load-bearing for any conclusion here.
- **Any live database query.** A worker may not read `~/Library/Application Support/Boss/`, so the 219/220 Opus figure, the 15.9% / 37.5% spend shares, and the measurement window are taken as given from the task description rather than independently reproduced. Where a code-level mechanism explains one of those observations, that is called out as an explanation, not a confirmation.
- **Whether any specific historical review ran on a non-Opus model.** One code path could produce that (see _The one leak_), but attributing the single measured non-Opus run to it would be a guess.

## Where the decision lives

The review pool's model is chosen at exactly one place. `pool_dispatch_policy_for_worker_id` (`engine/core/src/coordinator.rs:1546`) maps a worker id prefix to a driver and a tier, and today returns a constant for both the `review-` and `auto-worker-` prefixes: driver `claude` (`coordinator.rs:1503`), tier `PoolModelTier::Strong` (`engine/effort/src/lib.rs:294-301`). `Strong` resolves through the selected driver's menu via `model_for_reasoning(Investigation)` (`effort/src/lib.rs:334-337`), and for Claude that is `"opus"` (`engine/driver/src/claude.rs:59-64`).

The precedence order in `resolve_spawn_config_in` (`effort/src/lib.rs:332-351`) is what makes reasoning mode structurally unable to reach this decision:

| Step | Source                   | Reached for a `pr_review` execution?          |
| ---- | ------------------------ | --------------------------------------------- |
| 1    | `tasks.model_override`   | Yes — and this is the leak, see below         |
| 2    | pool tier override       | **Yes — always `Strong` for the review pool** |
| 3    | `tasks.reasoning`        | No — step 2 already returned                  |
| 4    | design-family kind floor | No                                            |
| 5    | effort-level table       | No                                            |
| 6    | `products.default_model` | No                                            |
| 7    | driver engine default    | No                                            |

So the measured observation — `pr_review` on Opus under reviewed rows marked `standard`, `investigation`, and unset alike — is not a coincidence in the data. It is the only outcome step 2 can produce. Reasoning mode is dead code on this path by construction.

### It was deliberate, and the reasoning is written down

The design doc states always-Opus as a **goal**, not an implementation detail: "a dedicated reviewer worker pool … always running at **Opus** level regardless of the reviewed task's effort" (design §Goals). Two of its rejected alternatives turn on it directly. A2 (self-review in the producing worker) is rejected in part because it "provides no model-level independence and no separate, higher-tier (Opus) perspective." A3 (dispatch review to the general pool) is rejected in part because "we could not give reviewers a per-pool Opus model override cleanly."

The mechanism was built for this purpose, not inherited: the design's own postmortem notes that the plan assumed the automation pool already had such an override to reuse, and it did not — automation was Opus only incidentally, and PR #1234 created the mechanism and pinned both pools. The later rework (#2515, `be0fe3e4`) that introduced `PoolModelTier` re-affirmed the policy in its commit message while fixing an unrelated bug: "Reviews stay on Opus by policy; what was wrong was resolving the model before the driver was known."

That is a decision, made twice, with a stated reason: **the reviewer's value comes from being independent of the producer, and one axis of that independence is capability.**

### What has changed since the decision

The design bounded always-Opus cost with a small pool: "default small (1–2) to bound concurrent Opus spend", shipped at 2 with a cap of 3. The pool is now 8, equal to the main pool (`coordinator.rs:203,208`). The design records this reversal and its consequence explicitly:

> ~~Review-pool slot count caps concurrent Opus spend~~ — _Divergence:_ with the pool at 8 (equal to the main pool) this control is no longer meaningfully bounding; the operator traded it for review latency. No per-day budget circuit breaker exists (explicitly deferred).

Its open-questions list is blunter still: cost ceiling is "Open, and sharper now that the pool sits at 8 always-Opus slots."

This is the honest version of the premise behind this investigation. It is not that the tier was never decided. It is that the tier was decided _together with_ a spend bound, and the bound was later removed on its own without the tier being revisited. The remaining controls are `max_review_cycles = 3` (`engine/core/src/config.rs:24`), the no-op gate (`config.rs:30`, defaulted to skipping literal no-ops only), and the severity gate.

### The one leak

`pool_dispatch_policy_for_worker_id` overrides the reviewed row's **driver** (`worker_spawn.rs:208-213`) but nothing overrides the reviewed row's **`model_override`**, which sits at precedence step 1 — above the pool tier. `compose_worker_spawn` passes the reviewed task's own `model_override` straight through (`worker_spawn.rs:648-649`, sourced at `:347-353`).

Two consequences follow mechanically, both from code reading only:

- A reviewed row carrying `model_override = "sonnet"` gets a **Sonnet reviewer**, silently, with no policy anywhere expressing that intent.
- A reviewed row carrying a non-Claude override (say a Codex model slug) resolves to driver `claude` + a Codex model, which the compatibility gate (`worker_spawn.rs:224-238`) then hard-fails — the reviewer never spawns.

This is the same class of bug #2515 fixed for `driver`, left unfixed for `model_override`. It is worth noting for a second reason: it means the _only_ existing way to get a non-Opus reviewer today is an unintended side channel on the reviewed row, which is not a usable experiment lever. Filed as a followup; not fixed here.

### One more thing the reviewer inherits

The reviewer's **runway** already varies with the reviewed row, even though its model does not. `row_effort` is passed to the resolver with no pool override (`worker_spawn.rs:648`), so the reviewer's `--effort` value comes from the reviewed row's `effort_level`: a reviewer of a `trivial` row runs at `--effort low`, a reviewer of a `max` row at `--effort max` (`claude.rs:36-44`).

The prompt addendum rides along too (`claude.rs:104-114`). A reviewer dispatched against a `large` or `max` row gets "Begin with a written plan. Identify the files you expect to touch and the order you'll touch them in" prepended to a **read-only** reviewer prompt that is otherwise explicit the reviewer must not touch anything.

Neither of these appears in the design. They read as artifacts of the pool override covering only the model half of the spawn config. Note the direction: effort tracks _size_, so today's reviewer gets more thinking budget for a big mechanical PR than for a small subtle one — the exact conflation the `reasoning` column was introduced to undo, reappearing on the review path through the back door.

## What review quality actually depends on

The rubric (`engine/pr-review/src/render.rs:564+`) is not one homogeneous task. It splits cleanly into two halves with different model sensitivity, and the split is visible in the code.

**Half one is mechanically assisted.** The engine pre-computes deterministic tripwires and injects them into the prompt as authoritative blocks the reviewer must dispose of: merged-parent deletions (`PrReviewContext::merged_parent_deletions`, rename/move-aware, computed engine-side), supersession-language hits in the PR narrative (`supersession_flags`), and bare Boss work-item id sweeps (`boss_construct_refs`) — all in `engine/pr-review/src/types.rs:40-70`. The engine also enforces the deletion tripwire independently of the reviewer's verdict: a non-empty set holds the task in `blocked: deletion_signoff` regardless of what the reviewer concluded (`completion/finalize_passes.rs:686-698`). For these, the model's job is closer to _disposition and phrasing_ than to _detection_. The incident-002 class — the highest-stakes failure mode the pool exists for — is the one with the strongest deterministic backstop.

**Half two is not.** Duplication requires searching the whole repo for an equivalent of a construct the diff introduces, from a description of what it does rather than a string to grep for. Supersession requires reading a cited design-doc section and judging whether it actually says what the PR claims. Deferred-scope requires reconciling the delivered diff against the work item's brief and deciding whether an undeclared gap exists. Architecture requires knowing the codebase's conventions well enough to tell "fights them" from "differs from them." These are the parts where a capability gap would plausibly show up, and where a cheaper model's most likely failure is not silence but **confident wrongness on a repo-wide claim** — a duplication finding naming the wrong existing module, or a supersession finding resting on a misread design section.

### The two failure modes are not symmetric in the machinery

A **missed defect** is silent and unbounded. Nothing downstream re-checks what the reviewer did not raise, except for the deterministic tripwires above. The PR advances to human Review and the cost is externalized.

A **plausible-but-wrong finding** is loud and immediately expensive: it mints a revision task on the main pool, that revision pushes, and with `enable_revision_triggered_reviews` on by default (`config.rs:39-46`) the push triggers another review. That is exactly the shape of "review plus revision-of-review = 37.5% of measured spend."

And the gate that decides whether a finding costs a revision is **blind to the reviewer's own stated confidence**. `passes_severity_gate` (`engine/pr-review/src/parsing.rs:146-159`) fires on any `critical`/`high` severity, or on any finding in `regression` / `duplication` / `deferred_scope` / `agent_isms` **regardless of severity**. `ReviewFindingConfidence` is defined (`types.rs:176-184`), collected on every finding (`types.rs:206`), rendered into the revision instructions for the worker to read — and consulted by no engine decision anywhere. A `low`-confidence `agent_isms` finding forces a full revision cycle exactly as hard as a `critical` correctness bug.

**This is the sharpest finding in this investigation, and it is orthogonal to the tier.** The system's stated defence against plausible-but-wrong findings is a signal it asks for and then discards. Whatever model runs review, wiring `confidence` into the gate is a strictly better lever than changing the tier: it is reversible, it targets the expensive failure mode directly, and its effect is measurable with data that already exists (revision counts by category).

## What the data can and cannot answer

**It cannot answer the tier question today.** Not "the numbers are noisy" — the records do not exist.

What is durable in `state.db`:

- `work_runs.model`, `output_tokens`, `input_tokens`, cache splits, `rounds`, `agent_active_ms` — added by #2440 (`f01a5b3a`, `work/migrations_a.rs:468-485`). This is the capture the task description's window starts at; there is no per-run cost data before it.
- `work_executions.kind = 'pr_review'`, carrying the **reviewed** item's `work_item_id` — attribution is exact, no PR heuristics needed.
- `tasks.review_cycle` and `tasks.last_reviewed_sha` on the review-cycle root (`work/migrations_b.rs:1627-1637`).
- `tasks.created_via = 'pr_review:<exec_id>'` on each minted revision — a durable review → revision link.

What is **not** recorded anywhere:

- **The findings themselves.** There is no findings table. `ReviewResult` is parsed at finalisation, rendered to prose in the revision description (`pr-review/src/render.rs:61+`), and the structured artifact is then reaped (`engine/structured-output/src/lib.rs:297-302`). Severity, category, and confidence survive only as text inside a task description, and only for reviews that passed the gate.
- **Clean reviews.** A review that found nothing qualifying produces no row at all — only a `review_cycle` increment and a tracing line. The denominator of any quality metric is missing.
- **What happened to each finding.** Nothing records whether the revising worker fixed it, disputed it, or found it wrong. The revision instructions ask the worker to "explicitly surface" a finding it is not fixing, but that lands as free text in a final response, parsed by nothing.
- **Whether a finding was right.** No human accept/reject signal on findings exists.

Transcripts are the only fallback and they are a poor one: `forensic-surfaces.md` measures Claude Code's own ~30-day cleanup at 100% survival ≤ 28 days, ~9.7% at 28–35 days, 0% beyond — with only 4,883 of 7,348 transcript paths still resolving, "roughly two-thirds of Boss run history is permanently cost-blind."

Two cautions on metrics that _look_ available:

- **`review_cycle` is not a quality metric.** More cycles could mean a sharper reviewer or a noisier one. Without per-finding disposition it does not have a sign.
- **`forensic-surfaces.md` is stale on one point.** It states "Per-task cost is **not** in `state.db`. No table holds tokens, model, or turn counts" — true when written, superseded by #2440. Anyone reaching for that doc to plan this measurement will conclude the wrong thing. Filed as a followup.

### What to instrument, in order

1. **Persist the review pass.** One row per completed `pr_review` execution: execution id, reviewed work item, head sha, resolved reviewer model, finding count, gate outcome, `revision_warranted`. Recording the model _on the pass_ (not only on `work_runs`) makes the tier the primary key of every later comparison and survives transcript loss. This alone turns "did any review run on the cheaper tier" from unanswerable into a one-line query.
2. **Persist findings as rows,** with a stable per-finding id: severity, category, file, title, confidence, and whether it individually cleared the gate. Cheap — the data is already parsed and validated at finalisation, then thrown away.
3. **Capture disposition.** Put the per-finding id in the rendered revision instructions and have the revising worker return a disposition per finding (fixed / disputed-with-reason / not-applicable). The `worker_proposals` seam already carries structured worker→engine returns and is the natural home; this is the only genuinely new capture of the four.
4. **Capture one human signal.** Even a coarse operator "this finding was wrong" on a revision card gives the false-positive rate a ground truth. Without it, steps 1–3 measure agreement between two agents, not correctness.

Steps 1 and 2 are prerequisites for any tier decision. Step 3 is what makes the plausible-but-wrong failure mode visible. Only after 1–3 does an A/B become interpretable — and note there is currently **no sanctioned way to run one**: `bossctl review start --pr <n>` re-enqueues through the same review pool and therefore the same constant policy (`bossctl/src/review.rs:17-36`). A tier experiment needs `pool_dispatch_policy_for_worker_id` to become a function of something, which is precisely what its own doc comment anticipates: "Follow-on work (configurable reviewer model, load balancing across reviewer models, two-party review with two distinct reviewers on one PR) only needs to change what this function returns."

## Is a split better than a single choice?

Four options, evaluated rather than assumed.

**Keep Opus everywhere.** Preserves the stated independence rationale and takes no risk on the failure mode with no backstop. Costs the most, and leaves the design's own "cost ceiling" open question open. **This is the recommendation for now** — with the qualification that it is a re-decision, and it should be revisited once the instrumentation above exists.

**Move all review to Sonnet.** The cheapest and the most reversible in code, but not in consequence: a regression that slips through and merges is not undone by flipping the constant back. The half-two rubric dimensions are exactly where a capability gap would land, and there is no measurement to detect it. **Reject on the current evidence** — as an unmeasurable change to the system's last automated defence, not as a claim that Sonnet is inadequate.

**Split on the reviewed row's `reasoning`.** Intuitive and wrong. `reasoning` describes what it took to _produce_ the change, not what it takes to _check_ it. A `standard` row can be a mechanical-looking forward-port that silently drops a feature — the incident-002 case, and the single hardest thing on the rubric. Coupling the reviewer's capability to the producer's self-assessment also re-introduces exactly the producer-dependence the pool was built to remove. **Reject on principle**, independent of cost.

**Split on a property of the change itself** (diff size, file count, subsystem, whether the deterministic tripwires fired). This is the only split with a defensible story: it keys on the reviewing task's difficulty rather than the producing task's. It is also mechanically cheap — the changed-file list and diff are fetched at spawn time and are already in hand _before_ model resolution runs in the same function (`worker_spawn.rs:600-655`), and the engine already computes an effective-changed-lines figure for the no-op gate. But "small diff ⇒ cheap review is safe" is an untested hypothesis, and small diffs are not obviously the easy ones — a three-line change to a merge path can be the subtlest thing a reviewer sees all week. **Best candidate, but not yet: it is a hypothesis to test once the instrumentation exists, not a change to make now.**

### What to do instead, now

Three things, none of which is a tier change, all cheaper and all reversible:

1. **Wire `confidence` into the severity gate.** Targets the plausible-but-wrong failure mode directly. The likely shape: a `low`-confidence finding in one of the forced categories should not be sufficient on its own. Measurable against revision counts by category with data that already exists.
2. **Fix the `model_override` leak** so the reviewer's model is genuinely a policy decision rather than a property the reviewed row can silently set, and stop the reviewer inheriting the reviewed row's effort value and planning addendum. Until this is fixed, "the review pool hardcodes Opus" is not strictly true, and any tier experiment sits on a contaminated baseline.
3. **Land the pass-and-findings persistence** (steps 1–2 of the instrumentation list). It is the prerequisite for ever answering the question this document could not.

Effort is deliberately absent from all of the above. Effort picks a worker's runway, not its model; using it as a model lever would distort scheduling and every effort-based report, and the fact that the reviewer _already_ inherits the reviewed row's effort is listed here as a defect to fix, not a mechanism to exploit.

## Follow-up code changes (for separate filing)

This investigation changed no code. Concrete follow-on work it identified:

1. **Reviewer inherits the reviewed row's `model_override`** — precedence step 1 outranks the pool tier, so a reviewed row can silently set its own reviewer's model, or make the reviewer fail to spawn outright when the override belongs to another driver's vocabulary. Same bug class as #2515, one field over.
2. **Reviewer inherits the reviewed row's `effort_level`** — reviewer runway tracks the reviewed change's _size_, and a read-only reviewer of a `large`/`max` row receives a "begin with a written plan, identify the files you expect to touch" addendum.
3. **`ReviewFindingConfidence` is collected and never consulted** — the severity gate is confidence-blind, so a `low`-confidence finding in a forced category costs a full revision cycle.
4. **No persistence of review passes or findings** — the structured artifact is reaped at finalisation and clean reviews leave no row, so review quality is unmeasurable and the tier question is unanswerable.
5. **`forensic-surfaces.md` is stale on per-run cost** — it states no table holds tokens or model; `work_runs` gained those columns in #2440.
