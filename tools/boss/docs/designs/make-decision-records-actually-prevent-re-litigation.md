# Make decision records actually prevent re-litigation

- **Date:** 2026-09-04
- **Status:** Design — awaiting review
- **Parent project:** Make decision records actually prevent re-litigation (Boss product)
- **Provenance:** `project_design` run; supersedes the "minimal app surface" that §T-B2-decision of `retire-the-coordinator-s-memory-make-the-defaults-teach-the-right-thing.md` planned and never built
- **Code home today:** `protocol/src/types/decision.rs`, `engine/core/src/work/decisions.rs`, `cli/src/decision_commands.rs`, `cli/tests/decision.rs` (all under `tools/boss/`)
- **Exemplar:** flunge `D1` ("Buildkite pipelines must not depend on a Buildkite API token"), the only decision in the system at the time of writing

Decision records shipped as a store with no readers. This design gives them three readers, each at a different cost and precision, and makes every collision with a standing ruling an attributable engine record rather than a stderr line.

## TL;DR

The central bet, stated so it can be disagreed with: **enforcement is a stack of three read points, and the mechanical one is the engine inlining the applicable decisions into every worker and reviewer prompt, not the coordinator's diligence.** The coordinator's consultation at brief time is the cheapest and the operator's preferred layer, and it is where the ask gets shaped, but it is a prompt rule an LLM can skip, and the exemplar violation was not visible in the ask at all. So the design builds all three, in this order:

1. **Coordinator, at brief time** — a prompt section that tells the coordinator decisions exist, when to record one, and to read the applicable list before filing anything that changes architecture, policy, topology, credentials or CI; plus an engine-side create gate that refuses a colliding create unless the caller acknowledges the specific decision by id, which writes a collision row.
2. **Worker, at approach-choice time** — the engine inlines every applicable active decision into the worker's initial prompt, with a directive to re-check the list at the moment its approach acquires a new credential, service, CI or policy dependency, and to stop with a `[blocked]` marker naming the decision instead of proceeding.
3. **Reviewer, against the diff** — the engine inlines the same applicable set into the reviewer prompt; the rubric gains a decision check; a new `decision_violation` finding category carries the decision id, forces a revision regardless of severity (the same forcing treatment as regression and duplication), and is written to the collision ledger.

Scope: the per-product `D<n>` namespace stays; global decisions are the same table with `product_id = NULL` and their own `G<n>` label. "Which decisions apply here" is one query: active rows where the product matches or the product is null.

Relevance: the deterministic matcher stays as the create gate (widened to read the work item's description and the decision's body), and no LLM matcher is added. The readers that need semantic judgement, the coordinator, the worker and the reviewer, are LLMs reading the _full_ applicable list, which is small by construction and must stay small.

Awareness (the meta-cause): a golden test that fails the build when a `boss` verb is mentioned in none of the three agent prompts and is not on an explicit human-only list with a reason, plus a reviewer rubric bullet for "new agent-facing surface with no prompt route". The test is the floor; the rubric is the judgement.

## Goals

- Standing rulings (kind `decided`) and declined proposals (kind `wontfix`) are consulted by the coordinator before work is filed, by the worker while choosing an approach, and by the reviewer against the diff.
- A work item that collides with an active decision and proceeds anyway leaves a visible, attributable record naming who acknowledged which decision and why.
- A decision can span products.
- Decisions are visible in the Boss app: the ruling, its rationale, what it supersedes and is superseded by, and which work items have collided with it.
- The coordinator, workers and reviewers know the mechanism exists without having to discover it in `boss --help`, and the next agent-facing surface cannot ship equally invisible.

## Non-goals

- **Not a bug tracker, not a memory store, not a TODO list.** A decision is a standing ruling that outlives work items. The coordinator prompt text in this design says so in as many words; nothing here adds fields that would make a decision look like a task (no status beyond active/superseded/revoked, no assignee, no due date).
- **No LLM-assisted relevance matching at create time.** Rejected below; the corpus is expected to stay in the tens, and a refusal path must be reproducible.
- **No decision authoring from the app.** Creating, revoking and superseding stay CLI operations performed by the operator or by the coordinator on the operator's say-so. The app surface in this design is read-only. This is a deliberate v1 boundary, not a deferral: the app is a thin client and the write path already exists.
- **Workers do not create decisions.** `CreateDecision` stays outside the worker-tier allow set. A worker that concludes a ruling is needed says so in its final report; the coordinator files it.
- **No change to the kinds vocabulary** (`wontfix`, `decided`) or to the lifecycle (`active`, `superseded`, `revoked`).
- **No enforcement inside CI or checkleft.** Decisions are Boss-side rulings about how work is filed and reviewed; a ruling that is also a repo invariant should additionally become a checkleft rule, but that is a per-ruling judgement, not a mechanism this design builds.

## What exists today, verified

Every claim below was checked against source or the running engine during this run.

- **Storage and RPC.** `product_decisions` table with a per-product `decision_short_id_sequences` counter (`engine/core/src/work/migrations_b.rs`, `dispatch_helpers.rs`). `CreateDecision`, `GetDecision`, `ListDecisions`, `RevokeDecision`, `SupersedeDecision` requests with `DecisionCreated` / `DecisionResult` / `DecisionsList` / `DecisionUpdated` replies. `ListDecisions` takes a product id and an include-inactive flag.
- **The only consumer** is `warn_if_overlapping_decision` in `cli/src/decision_commands.rs`, called from the task, chore and automation create paths. It lists active decisions for the product, tokenises the new _name_ against the decision's _title plus keywords_, and prints a stderr warning when at least two significant tokens overlap and Jaccard is at least 0.5. It never reads the work item's description or the decision's body. Against the exemplar it is silent: "Fail CI when Buildkite stored config drifts from a stub" shares one significant token ("buildkite") with the title of `D1`.
- **Workers can already read decisions.** `GetDecision` and `ListDecisions` are in the worker-tier Allow arm of `worker_verb_decision` (`engine/worker-policy/src/policy.rs`), alongside `GetWorkerContext`. Confirmed end to end from this worker session: `boss decision list --product boss --json` returned `{"decisions": []}` with exit 0, and the same call against flunge returned `D1`. The list is not scoped to the caller's own product, so a worker can read any product's decisions. **Gap 2 therefore needs no new read plumbing.** What is missing is that nothing tells a worker the verb exists, and nothing puts the text in front of it.
- **The coordinator prompt never mentions decisions**, and two of its existing rules point the other way. The "Filing and briefs" rule "Do not file fixes for deliberate design choices" tells the coordinator to check "config comments, docs, git history, or a direct ask" — the exact list that a decision record should head, and it is absent. The session-handoff example carries a `## Decisions` section, teaching the coordinator that a ruling belongs in the handoff, which is session-scoped and rewritten on every world-state change. Neither is a lost reason; the prompt surgery that shipped decisions (`#2439`) and the decision CLI (`#2438`) simply never met.
- **The worker prompt composer** (`engine/core/src/runner/prompt.rs`) and **the reviewer prompt** (`engine/pr-review/src/render.rs`) contain no decision text and no reference to the verb. The reviewer rubric's forced-revision categories are regression, duplication, deferred scope and agent-isms (`passes_severity_gate` in `parsing.rs`); there is no category for a policy violation.
- **`boss context`** (`WorkerContextBundle`, `protocol/src/types/context.rs`) carries task, project, product, sibling tasks, own dependencies, attention groups and proposals. No decisions.
- **No app surface.** The app has Designs and Automations tabs (`ContentView.swift`) and a `CapabilityRegistry.swift`; nothing decodes a decision.
- **One decision exists** in the whole system (flunge `D1`, kind `decided`, created via CLI by the operator). Boss, appoint, checkleft-sandbox and Test Product have none. Every short-id namespace in Boss (`T<n>`, `A<n>`, `D<n>`, automations) is per product, per the friendly-numeric-ids design's Q1.

## Alternatives considered

### Scope model

**A. Single global `D<n>` counter, product scoping as an attribute.** One label vocabulary, no cross-scope pointer question, and today's migration cost is zero because `D1` is the only row. Rejected because every other short-id namespace in Boss is per product by an explicit recorded choice (friendly-numeric-ids §Q1), the ambiguity of a bare `D<n>` across products is the same ambiguity `T<n>` already has and is resolved the same way (product context in chat), and the migration cost is zero only until the first product-scoped decision is filed under the old counter, which could be any day between this doc and the migration landing.

**B. A separate `global_decisions` table.** Rejected: it duplicates the lifecycle and the supersession pointer, and a product decision superseded by a global one (the likely direction: a ruling first made for flunge later generalised) would need a polymorphic `superseded_by`.

**C (chosen). Same table, `product_id` nullable, global rows labelled `G<n>` from their own counter.** One applicability query, one lifecycle, cross-scope supersession by canonical id, and the per-product convention untouched.

### What "enforced" means at create time

**Stronger advisory only** (stderr today, or an attention item). Rejected: an advisory to the row's creator is what shipped, and the creator is the coordinator, whose stderr nobody reads. Moving it to an attention item makes it visible to the operator after the fact but still lets the row dispatch. This is the failure mode the project exists to fix.

**Hard block, no override.** Rejected: the coordinator legitimately files work that revisits a decision (the operator asks for `D1` to be reconsidered, or a task whose purpose is to implement the superseding approach). A block with no override forces revoking the decision first, which destroys the record of the ruling precisely when the ruling is under discussion. It would also disqualify the existing duplicate-create guard, which is the same shape with `--force-duplicate`, and that guard is established practice.

**Refuse unless the specific decision is acknowledged by id, with a reason, and record the acknowledgement (chosen).** Same shape as the duplicate guard, but the override names the decision, carries a reason, is attributed to the caller, and is stored. It cannot be silent because the refusal is a typed error the coordinator has to handle, and it cannot be anonymous because the acknowledgement is a row.

### Relevance matching

**Keep the current lexical matcher unchanged.** Rejected: it demonstrably misses the exemplar and reads neither the description nor the body, which is where the words that matter live.

**Add an LLM relevance judge at create time** through the engine's `UtilityModel` seam (precedent: the comment-intent classifier and the attentions backstop). Rejected for the _gate_ specifically: a refusal that depends on a model call is not reproducible (the same create can pass on retry), it adds a network round trip and a credential dependency to every `boss task create`, and it answers the wrong question. Whether a decision is _relevant_ to a piece of work is a judgement that the coordinator, worker and reviewer must make anyway while reading the full list, and they are already LLMs. The value of an automated matcher is only as a tripwire that fires when the coordinator has _not_ read the list; a deterministic tripwire over widened inputs is enough for that.

**Deterministic matcher over widened inputs as the gate; full-list reading by the LLM readers as the judgement (chosen).** Stated at the load-bearing level: the property the system needs is that _every reader that can act on a decision has the whole applicable list in front of it_, not that a matcher picks the right subset. The matcher exists only to catch a create that skipped the reading.

### How the awareness class is closed

**A checklist item in the PR template or CLAUDE.md** ("did you update the prompts?"). Rejected as the only mechanism: silence is not neutral, and a checklist a worker can tick without doing the work is an advisory nobody reads.

**Generate the prompt's verb listing from the CLI definitions** so every verb is visible by construction. Rejected as the whole answer: the coordinator _did_ eventually find `boss decision` in `--help`, months late, because a listing tells you a verb exists but not when to reach for it. The trigger rule is the load-bearing part.

**A build-breaking coverage test plus a reviewer rubric bullet (chosen).** The test guarantees the floor: no verb can ship unmentioned. The rubric bullet asks the reviewer to check that the mention is a trigger rule, not a name-drop. Neither alone is sufficient; together they are cheap.

## Chosen approach

### Scope model

- `product_decisions.product_id` becomes nullable. A row with `product_id IS NULL` is a **global decision**.
- Global rows draw their short id from `decision_short_id_sequences` under the reserved scope key `global`, and display as `G<n>`. Product rows keep `D<n>`. `decision_short_id_label` takes the scope into account; `display_label` on `Decision` renders the right prefix.
- **Applicability** is defined once, in the engine: the active decisions that apply to product `X` are `status = 'active' AND (product_id = X OR product_id IS NULL)`. Every reader in this design calls that one query; no reader re-derives it.
- `ListDecisions` gains `scope`: `product` (current behaviour), `global` (only global rows), or `applicable` (the union above, the default for every agent-facing caller). The CLI exposes `boss decision list --product <p>` (now applicable by default), `--global` (global rows only), and `--product-only`.
- `boss decision create --global` creates a global row; `--product` and `--global` are mutually exclusive and one is required.
- Selector parsing accepts `G<n>` (no product needed), `D<n>` (product required, as today), and canonical `dec_…` ids. `supersede` accepts any mix: a product decision may be superseded by a global one and vice versa, since the pointer is the canonical id.
- Migration is a table rebuild (SQLite cannot drop `NOT NULL` in place): create the new table, copy rows, drop, rename, recreate the two indexes. `D1` keeps its id, short id and product.

### Relevance and the create gate

- The matcher moves out of the CLI into a small engine crate (`tools/boss/engine/decision-match`, one-way edge from `engine/core`), per the crates-over-modules convention. It keeps the current predicate shape (significant tokens, minimum intersection, Jaccard floor) but widens both sides: the candidate side is the new work item's **name plus description**; the decision side is **title plus keywords plus body**. Thresholds are re-tuned in the crate's tests against the exemplar so that the flunge task's actual description (which names Buildkite, stored config, drift, the API and a token) collides with `D1`, and the near-miss fixtures from today's tests stay quiet.
- `CreateTaskInput`, `CreateChoreInput`, `CreateInvestigationInput` and the automation-materialised create path gain `acknowledged_decisions: Vec<DecisionAcknowledgement { decision_id, reason }>` (builder default empty, so no construction site changes).
- The engine create handlers run the matcher against the applicable set **before insert**. If any active decision matches and is not in `acknowledged_decisions`, the create is refused with a typed `DecisionCollisionError` listing each matched decision (id, label, title, kind) — the same treatment as `DuplicateTaskError`. Nothing is written.
- A create that supplies acknowledgements for every match proceeds, and for each acknowledgement the engine writes a **collision ledger** row (below) with `source = create_acknowledged`, the caller's attribution (`created_via`, `created_by` as resolved by the engine, never caller-asserted), and the reason. An acknowledgement for a decision that did not match is rejected as a caller bug, so the flag cannot be used as a blanket bypass.
- CLI: `boss task create --acknowledge-decision <D<n>|G<n>|dec_id> --acknowledge-reason "<why>"`, repeatable; the reason is required. `boss chore create` and `create-investigation` get the same flags. The error rendering names the decisions and the exact flag to re-run with, as the duplicate guard does.
- The CLI-side stderr warning is **deleted in the same PR that lands the engine gate**, and not before. The engine gate is strictly stronger (it reads more, refuses instead of warning, and fires for every caller including the materializer and automations), so removing the warning at that moment does not weaken the surface. The existing CLI test `task_create_warns_on_decision_overlap_without_breaking_json` pins the premise that the warning is the surface; it is replaced in the same PR by an engine test that the create is refused and a CLI test that `--json` stdout stays valid on refusal.
- Engine-internal creates that must not be refused (the planner materializer creating rows from an approved design doc, automation-materialised rows) are not exempt: the materializer already passes `force_duplicate(true)`, and it must instead **fail the materialisation for that row and raise an attention item** naming the decision, because a design doc that contradicts a standing ruling is exactly the case the operator wants to see. This is a behaviour change for the populator and is called out in the task breakdown.

### Enforcement point 1: the coordinator at brief time

A new `## Standing decisions` section in `bossSystemPrompt` (the Swift literal in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift`; never the runtime `CLAUDE.md`), placed immediately before "Judgement rules" so the filing rules can refer back to it. Its content, in substance:

- What a decision is and is not: an operator-owned standing ruling (`decided`) or a considered-and-declined proposal (`wontfix`) that outlives any work item; not a note, not a TODO, not deferred work, not a bug report. Product-scoped `D<n>` or global `G<n>`.
- **When to record one, in the same turn:** whenever the operator states a rule in standing terms ("always", "never", "must not", "we don't do X", "the plan is X") that binds future work rather than the current ask. Ask the operator to confirm the wording once, then `boss decision create --kind decided` (or `wontfix` when the operator declines a proposal on the record). Record the _why_ in the body; the body is what the worker and reviewer will read.
- **When to consult:** before filing anything that changes architecture, topology, policy, credentials, secrets, CI, or an external dependency, run `boss decision list --product <p> --json` (applicable scope) and read it. Cite any decision that bears on the ask in the brief by label, with one sentence on _why it applies to this ask_, so the worker starts with the constraint and its reasoning.
- **On a collision refusal:** report the refusal to the operator verbatim with the decision's title. Pass `--acknowledge-decision` only when the current ask explicitly revisits or supersedes that decision, and say in the reason which ask authorised it. Never acknowledge to make a refusal go away.
- The existing "Do not file fixes for deliberate design choices" bullet is amended so `boss decision list` heads its list of places to check. The handoff example's `## Decisions` section is renamed `## Session-scoped calls` with a one-line note that a standing ruling goes in `boss decision create`, not the handoff.

The coordinator's brief-time consultation is the operator's preferred layer and the cheapest one. It is deliberately _not_ the only one, because it catches violations implied by the ask and the exemplar's violation was not in the ask.

### Enforcement point 2: the worker at approach-choice time

- The engine's worker prompt composer renders a `## Standing decisions` block into every implementation-kind initial prompt (task, chore, investigation, revision, conflict resolution, CI remediation, design): the applicable set for the work item's product, each as label, kind, title and body. If the rendered block would exceed a fixed budget (8 KiB is proposed), it degrades to label, kind and title per decision plus the instruction to run `boss decision show <label>`; that fallback must itself be visible in the block ("N decisions truncated to titles"), never silent.
- The block carries the directive that closes the exemplar: _"Re-read this list at the moment your approach acquires a dependency the brief did not name: a credential or secret, an external service, a CI step, a schema or topology change, a policy. If the approach you are about to take collides with a decision here, do not proceed and do not work around it. Stop with `[blocked] reason="collides with D<n>: …"` (or `boss propose blocked --decision D<n>` where the proposal seam is enabled) so the operator can revisit the ruling or the approach."_ The `[blocked]` marker and `propose blocked` already exist; this adds an optional `decision_id` to the blocked proposal so the engine can write a ledger row with `source = worker_blocked`.
- `boss context` gains `decisions: Vec<Decision>` (the applicable active set) so a worker can re-read after context compaction without knowing the product id. The worker CLAUDE.md template names `boss decision list` and `boss decision show` in its read-only verbs.
- Nothing else changes for workers: the read RPCs are already allowed. Verified, see above.

### Enforcement point 3: the reviewer against the diff

- The engine passes the applicable decision set into `render_reviewer_initial_prompt` and `render_batch_reviewer_initial_prompt` as data (the pr-review crate must not depend on the engine; it renders what it is handed). The prompt gains a `## Standing decisions` section and the code rubric gains a bullet: check every changed line against each decision; a finding is warranted only when the diff itself establishes the violation, cited at file and line as with any other finding, and confidence is stated. The docs-only rubric gets the same bullet; a design doc can propose a violating approach.
- `ReviewFindingCategory::DecisionViolation` (`decision_violation` on the wire) with a required `decision_id` field on the finding. The parser rejects a `decision_violation` finding whose `decision_id` is not an active applicable decision, so the category cannot be used for a reviewer's own opinions.
- `passes_severity_gate` adds `DecisionViolation` to the forced-revision set regardless of assigned severity, joining regression, duplication, deferred scope and agent-isms. The revision instructions render it under its own heading ("Standing decision violated"), quoting the decision's title and label, so the revision worker is told what rule it is up against rather than what line to change.
- On accepting a report with such a finding, the engine writes a ledger row with `source = review_finding`, the decision id, the reviewed work item, the PR URL and head SHA.
- False positives at scale are handled by three things, none of them a matcher: the applicable set is small and must stay small (the coordinator prompt says a decision is a ruling, not a note); a finding needs a diff citation; and a wrong finding costs one revision cycle in which the worker may take the disagreement path below.

### The disagreement path

A reviewer flags `D<n>`; the revision worker believes the decision is wrong or does not apply. The worker does not argue in the PR and does not work around the finding. It emits `[blocked] reason="reviewer flagged D<n>; I believe … because …"` (or the blocked proposal with `decision_id`). That is already an attention item for the coordinator, who brings it to the operator. The operator either upholds (the revision proceeds under the ruling), `boss decision revoke`s, or `boss decision supersede`s with a new record whose body states the new ruling and why. After a revoke or supersede, the next review pass does not re-raise the finding, because the decision is no longer applicable. Nothing new is built for this path; it is the composition of existing pieces, and the design's job is to say so in the worker and reviewer prompts.

### The collision ledger

A new table, `decision_collisions`, owned by the engine:

| column               | meaning                                                                                                                        |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `id`                 | primary key                                                                                                                    |
| `decision_id`        | the decision collided with (FK to `product_decisions`)                                                                         |
| `work_item_id`       | the task or chore that collided (soft reference, as `related_work_item_id` is today)                                           |
| `source`             | `create_acknowledged` \| `worker_blocked` \| `review_finding` \| `materializer_refused`                                        |
| `actor`              | engine-resolved attribution: `created_via` and `created_by` of the create, or the execution id for worker and reviewer sources |
| `reason`             | the acknowledgement reason, the blocked reason, or the finding's summary                                                       |
| `pr_url`, `head_sha` | set for `review_finding`                                                                                                       |
| `created_at`         |                                                                                                                                |

Rows are append-only. `ListDecisionCollisions { decision_id }` and `ListDecisionCollisionsForWorkItem { work_item_id }` are new read RPCs, in the worker-tier Allow set (reads only). A `DecisionCollisionRecorded` event is emitted so the app updates live. This table is what makes the "visible and attributable" constraint true at the level of the property rather than the container: every path that proceeds past a decision writes here, and the UI reads only from here.

### UI

- A **Decisions** tab beside Designs and Automations in `ContentView.swift`, following the `AutomationsView` pattern. The list shows label, scope (product name or "Global"), kind, status, title, created by and date; filters for scope and status; inactive rows hidden by default.
- The detail view shows the body rendered as markdown (the rationale is the body; the design does not add a separate field), the supersession chain in both directions (the predecessor is found by querying `superseded_by = this id`), `related_work_item_id` as a link, and the **collisions list** from the ledger: each row as source, work item (linked to the card), actor, reason, date.
- Work cards for items with any ledger row show a small badge ("collides with D3"), consistent with the existing badge strip, so a collision is visible where the operator already looks.
- App-side this is decoding `Decision` and the ledger row, three read RPCs, and the two events. No app-side logic decides applicability or relevance.

### Closing the awareness class

The instance: the prompt changes above. The class:

- **A prompt-coverage test** in the engine's test tree, run by `bazel test //tools/boss/...`: it enumerates every `boss` verb and subverb from the clap definitions in `cli/src/commands.rs`, reads the three agent-facing prompt sources as data (the coordinator Swift literal, the worker prompt fragments in `runner/prompt.rs` and the worker CLAUDE.md template, and the reviewer prompt in `pr-review`), and fails when a verb appears in none of them and is not in an explicit `HUMAN_ONLY_VERBS` list, where every entry carries a one-line reason. `handoff` is coordinator-only and would be on the coordinator prompt already; verbs like `shake` are already there. The test's failure message says which prompt the verb should probably join.
- **A reviewer rubric bullet** under the existing architecture heading: a PR that adds or changes an agent-facing surface (a `boss` verb, a `boss context` field, a marker, a proposal kind, a worker-readable file) without adding its trigger rule to the prompt that should use it, or without stating in the PR body why no agent needs it, is a `high` architecture finding. The test guarantees the name is mentioned; this asks whether the mention says _when_.
- **A rule in the repo's worker instructions** (`tools/boss` CLAUDE.md) stating the same in one paragraph, so implementers see it before the reviewer does.

The invariant, stated at the level that is load-bearing: _no agent-facing surface can be merged without a route from an agent's prompt to it._ The test enforces existence of the route; the review enforces that the route is a rule.

### Wire and schema summary

- Schema: `product_decisions.product_id` nullable (table rebuild); `decision_short_id_sequences` gains the `global` key; new `decision_collisions` table with indexes on `decision_id` and `work_item_id`.
- Protocol: `Decision.product_id: Option<String>`; `DecisionScope` enum for `ListDecisions`; `DecisionAcknowledgement`; `acknowledged_decisions` on the three create inputs and on the blocked proposal; `DecisionCollision` type; `ListDecisionCollisions`, `ListDecisionCollisionsForWorkItem`; `DecisionCollisionRecorded` event; `decisions` on `WorkerContextBundle`; `decision_violation` finding category with `decision_id`.
- Engine: `decision-match` crate; applicability query in `work/decisions.rs`; create-gate in `insert_helpers.rs` beside the duplicate guard; ledger writes in the create handlers, the blocked-proposal apply path, the review-report accept path, and the materializer; prompt blocks in `runner/prompt.rs`; reviewer data passed into `pr-review` renderers; the coverage test.
- CLI: `--global`, scope flags on `list`, `G<n>` selectors, `--acknowledge-decision` and `--acknowledge-reason` on the create verbs, `propose blocked --decision`; the stderr warning removed with the gate.
- App: Decisions tab and detail, collision list, work-card badge, event handling.
- Prompts: coordinator section and two amendments; worker block and directive; reviewer section, rubric bullet and revision rendering.

## Risks and open questions

- **Friction calibration.** Refuse-with-acknowledgement is the middle option and the one this design defends, but whether the matcher's widened inputs produce refusals the coordinator finds tolerable is an empirical question. The crate's tests pin the exemplar and today's near-misses; the first month of collisions in the ledger is the real study, and it is a _validation_ of the chosen thresholds, not a comparison between gate designs. If refusals are noisy the fix is thresholds or keywords, not removing the gate.
- **Corpus growth.** Every layer here reads the full applicable list. If a product accumulates dozens of decisions the prompt blocks bloat and the reviewer's precision falls. The coordinator prompt's "ruling, not note" framing is the control; the truncation fallback is the backstop. A per-product count is worth a metric so the operator sees it before it hurts.
- **The materializer behaviour change.** Failing a planner-materialised row on collision and raising an attention item is the right call by the constraints, but it changes an existing idempotent path; the populator's tests need to cover the partial-materialisation case.
- **Reviewer over-reach.** A reviewer told to hold rulings may stretch a ruling past its wording. The `decision_id` validation, the diff-citation requirement and the disagreement path bound the cost, but the operator should expect a few early findings that lead to a `supersede` with tighter wording, which is the system working.
- **The coverage test reads a Swift file from Rust.** It is a text fixture, not a Swift build dependency; if the prompt literal ever moves to a resource file the test's data label moves with it. Whether the test should also cover `bossctl` verbs for the coordinator is a small open question; the design proposes yes, with the same human-only list.
- **Automation-created rows.** Automations already run the CLI warning path; under the gate their creates can be refused. The automation run should fail with the typed error and its triage should surface it, which is the existing failure path for automation creates and needs no new mechanism, but should be confirmed in the gate task.

## Proposed implementation task breakdown

Breakdown size: 7 entries (7 in-scope, 0 deferred) — the change has one schema seam (global scope), one engine seam (the applicability query, matcher and ledger with its create gate), three prompt-and-plumbing readers (worker, reviewer, coordinator), one app surface, and one cross-cutting guard, and worker reads need no new plumbing because the RPCs are already worker-tier, which is why this lands at seven rather than the ten-plus a three-subsystem feature would otherwise suggest.

Parallelism: after "Global decision scope" lands, "Engine create gate and collision ledger" and "Worker prompt inlining and approach-time directive" may run in parallel (they touch different engine files: `insert_helpers.rs` / `decisions.rs` versus `runner/prompt.rs` / `app/context.rs`; both touch `protocol/src/types` but different files). "Reviewer decision check" and "Coordinator prompt" both depend on the gate and may run in parallel with each other. "Decisions tab in the app" depends on the gate for the ledger RPCs and may run in parallel with the reviewer and coordinator entries. "Prompt-coverage test" is last.

### Global decision scope

Scope: make `product_decisions.product_id` nullable via a table-rebuild migration; allocate global short ids under the reserved `global` sequence key and render them as `G<n>`; add `DecisionScope` to `ListDecisions` with `applicable` as the agent-facing default and implement the single applicability query in `work/decisions.rs`; update `Decision.product_id` to `Option<String>` and `display_label`; CLI `boss decision create --global`, `list --global` / `--product-only`, and `G<n>` selector parsing; allow cross-scope `supersede`. Existing round-trip CLI test extended for a global row and for applicable listing. `D1` must survive unchanged (id, short id, product).

Effort: medium

Dependencies: none

Scope: in-scope

### Engine create gate and collision ledger

Scope: extract the matcher into a new `tools/boss/engine/decision-match` crate with widened inputs (name plus description against title, keywords and body) and thresholds pinned by tests against the exemplar and today's near-miss fixtures; add the `decision_collisions` table, `DecisionCollision` type, the two list RPCs (worker-tier allowed) and the `DecisionCollisionRecorded` event; add `DecisionAcknowledgement` and `acknowledged_decisions` to the task, chore and investigation create inputs; run the gate in the engine create handlers before insert, refusing with a typed `DecisionCollisionError` and writing `create_acknowledged` ledger rows on acknowledged proceeds; reject acknowledgements that did not match; make the planner materializer fail the colliding row and raise an attention item (`materializer_refused`); CLI `--acknowledge-decision` / `--acknowledge-reason` on the three create verbs with error rendering that names the re-run flag; delete the CLI stderr warning and replace its test with the engine refusal test and a `--json`-stdout-stays-valid test in this same PR.

Effort: large

Dependencies: Global decision scope

Scope: in-scope

### Worker prompt inlining and approach-time directive

Scope: render a `## Standing decisions` block (applicable set, budgeted with a visible truncation fallback) into every implementation-kind initial prompt in `runner/prompt.rs`, carrying the approach-time re-check directive and the `[blocked]` stop instruction; add `decisions` to `WorkerContextBundle` and populate it in the `GetWorkerContext` handler; name `boss decision list` / `show` in the worker CLAUDE.md template's read-only verbs; add an optional `decision_id` to the blocked proposal and `propose blocked --decision`, and write a `worker_blocked` ledger row when it is set. Compose-prompt tests pin the block's presence per execution kind and the truncation fallback.

Effort: large

Dependencies: Global decision scope (applicability query); the ledger write depends on Engine create gate and collision ledger, so this entry lands after it or forward-ports the ledger write in a preserving way if it lands first

Scope: in-scope

### Reviewer decision check

Scope: pass the applicable decision set from the engine into the reviewer and batch-reviewer prompt renderers as data; add the `## Standing decisions` section and the rubric bullet to both the code and docs-only rubrics; add `ReviewFindingCategory::DecisionViolation` with a required `decision_id`, parser validation that the id is an active applicable decision, and its addition to `passes_severity_gate`'s forced-revision set; render it distinctly in the revision instructions; write a `review_finding` ledger row when a report carrying one is accepted; state the disagreement path in the reviewer and revision prompts. Fixture-based tests for the rubric text, the gate, and the invalid-id rejection.

Effort: large

Dependencies: Engine create gate and collision ledger

Scope: in-scope

### Coordinator prompt: standing decisions

Scope: edit `bossSystemPrompt` in `tools/boss/app-macos/Sources/Ghostty/BossPaneModel.swift` (never the runtime `CLAUDE.md`): add the `## Standing decisions` section (what a decision is and is not, record-in-the-same-turn trigger, consult-before-filing rule with the trigger list, brief citation form, collision-refusal handling and the acknowledgement rule); amend the "Do not file fixes for deliberate design choices" bullet to put `boss decision list` first; rename the handoff example's `## Decisions` section and add the one-line redirect to `boss decision create`. Prompt-only PR.

Effort: small

Dependencies: Engine create gate and collision ledger (documents `--acknowledge-decision`); Global decision scope (documents `--global`)

Scope: in-scope

### Decisions tab in the app

Scope: decode `Decision` (nullable product) and `DecisionCollision`; add the Decisions tab beside Designs and Automations following the `AutomationsView` pattern with scope and status filters; the detail view with markdown body, supersession chain in both directions, related work item link and the collisions list; handle `DecisionCreated` / `DecisionUpdated` / `DecisionCollisionRecorded`; add the work-card collision badge. Verify with an isolated capture instance and attach the PNG. App-only PR; no applicability or relevance logic app-side.

Effort: medium

Dependencies: Engine create gate and collision ledger (ledger RPCs and event); Global decision scope

Scope: in-scope

### Prompt-coverage test and reviewer rubric for agent-facing surfaces

Scope: add an engine test, run under `bazel test //tools/boss/...`, that enumerates `boss` (and `bossctl`) verbs from the clap definitions and fails when a verb is mentioned in none of the coordinator prompt source, the worker prompt fragments and CLAUDE.md template, or the reviewer prompt, and is absent from an explicit `HUMAN_ONLY_VERBS` list whose every entry carries a reason; add the "agent-facing surface without a prompt route" bullet to the reviewer's architecture rubric; add the one-paragraph rule to the `tools/boss` worker instructions. Lands last so the `decision` verbs are already covered and the test is green on arrival.

Effort: medium

Dependencies: Worker prompt inlining and approach-time directive; Reviewer decision check; Coordinator prompt: standing decisions

Scope: in-scope
