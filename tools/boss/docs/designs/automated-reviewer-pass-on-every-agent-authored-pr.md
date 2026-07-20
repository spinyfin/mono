# Design: Automated Reviewer Pass on Every Agent-Authored PR

## Status

Shipped (v1). All 13 planned tasks landed 2026-06-01 → 2026-06-03 as PRs
#1199, #1209, #1233, #1234, #1235, #1239, #1259, #1263, #1279, #1296,
#1302, #1308 (project P992), with post-ship hardening through 2026-07
(#1299 pool resize, #1320 slot-range unification, #1329 spawn-failure
hardening, #1712 revision-triggered reviews + chain-root cycle tracking).

This document was revised 2026-07-20 (design postmortem) to describe the
system **as built**. Where implementation diverged from the original plan,
the divergence and its reason are noted inline as _Divergence:_ notes.
Remaining gaps between this design and current `main` are collected in
_Known gaps_ near the end.

## Problem / goal

Today an agent-authored PR ships straight to the Review column with no
independent check. The producing worker is the only agent that ever looks
at its own diff, and self-review is weak: workers rationalize their own
choices. We have already been bitten by this — in the #1043 / T793
forward-port, a worker silently dropped a live feature during conflict
resolution and then rationalized the deletion as intentional. Nothing in
the pipeline caught it.

We want a mandatory, independent reviewer pass: every time a worker
**creates or updates** a PR, a _separate_ dedicated reviewer agent reviews
that PR read-only and produces high-bar, actionable feedback. If the
reviewer has feedback, it becomes a Boss **revision** on the original task.
Feedback stays **internal to Boss** — it is never posted to GitHub.

The goal is to raise the quality of all produced PRs (correctness,
architecture, readability, tests, edge cases, and specifically
inadvertent regressions from conflict resolution / forward-ports) while
**balancing speed** — a review pass must not excessively lengthen PR
turnaround.

## Goals

- **Independent review of every agent-authored PR.** On every PR create or
  update by a worker, a separate reviewer agent reviews the diff.
- **High-bar, actionable feedback** across correctness/bugs, architecture,
  code quality & readability, test coverage, edge cases, and — as a
  first-class explicit check — **inadvertent deletions / regressions**
  introduced during conflict resolution or forward-ports.
- **Feedback stays internal.** Review output becomes a Boss revision task,
  never a GitHub comment. No internal taxonomy, jargon, or blunt critique
  leaks onto public GitHub surfaces.
- **A dedicated reviewer worker pool**, modeled on the existing automation
  pool, always running at **Opus** level regardless of the reviewed task's
  effort, with its own execution kind and dispatch routing.
- **Read-only enforcement.** The reviewer cannot mutate the PR/branch — by
  prompt mandate and by tool denylist. The reviewer has a real workspace
  checked out to the PR head so it can read full source in context; writes
  remain denied via the tool denylist (with one sanctioned exception: the
  structured-output artifact, see _Read-only enforcement_).
- **Bounded cost and latency.** Explicit, tunable cap on review→revision
  cycles; skip re-review of no-ops; prefer fast high-signal feedback over
  exhaustive analysis.
- **Reviewer visibility** via a dedicated page on the macOS app's agents
  view, mirroring the automation-pool page.

## Non-goals

- **Posting review feedback to GitHub.** Explicitly out of scope and, in
  fact, prohibited. This design deliberately sidesteps the deferred
  "external PR comments" mechanism by keeping all feedback inside Boss as
  revisions. (See _Alternatives considered_.)
- **Replacing human review or CI.** The reviewer pass is an additional,
  internal quality gate before a PR reaches the Review column for a human;
  it does not gate merge, does not run tests itself, and does not replace
  the existing CI green-ness check the engine already performs.
- **Reviewing non-agent / human-authored PRs.** Scope is agent-authored
  PRs produced by Boss workers only.
- **Reviewing design / investigation doc PRs for code quality.** These are
  in scope for a _light_ review (see _Which PRs are in scope_) but the deep
  code rubric (tests, regressions, architecture) does not apply to a pure
  markdown deliverable.
- **A general-purpose, configurable review-rubric DSL.** v1 bakes the
  rubric into the reviewer prompt (mirroring how `editorial.rs` baked in
  defaults first). A configurable policy layer is future work.
- **Fixing the editorial control layer (P576) or the revision nudge-loop
  bug (T955).** This design _depends on / coordinates with_ those but does
  not subsume them.

## Alternatives considered

### A1 — Post review feedback as GitHub PR comments (rejected)

The most obvious design: have the reviewer post inline/PR comments on the
GitHub PR, and let the producing worker pick them up like human review
comments (the `"check comments"` flow).

Rejected because:

- It **leaks internal review chatter** (Boss taxonomy, blunt critique,
  internal reasoning, severity labels) onto a public GitHub surface that
  other humans see. This is the exact concern that originally caused us to
  _defer_ the whole feature.
- It creates a hard dependency on a mechanism for attaching comments
  _outside_ GitHub that does not yet exist.
- It would conflict with the editorial controls (P576), which exist
  precisely to police what agents say on GitHub.

The internal-revision approach gets the quality benefit with none of the
leakage, and needs no new GitHub-comment infrastructure.

### A2 — Self-review in the producing worker (rejected)

Add a "now review your own diff against main" step to the producing
worker's prompt before it opens the PR.

Rejected because:

- **Self-review is weak.** The producing worker is the same agent that
  made the choices; it rationalizes them. The T793 incident is a concrete
  case where a worker defended a deletion it should have flagged.
- It provides no model-level independence and no separate, higher-tier
  (Opus) perspective.
- It muddies the producing worker's role and makes its prompt longer and
  its turn slower with no independent check.

A separate agent with a fresh context and a review-only mandate is the
point.

### A3 — A single shared worker pool with a review execution kind (rejected)

Add a `pr_review` execution kind but dispatch it to the _existing_ general
worker pool rather than a dedicated pool.

Rejected because:

- Reviews would **compete with production work** for the same slots,
  coupling review latency to production backlog and vice versa.
- We could not give reviewers a **per-pool Opus model override** cleanly —
  model selection is currently effort-driven per task; a dedicated pool is
  the natural place to force a model regardless of effort.
- Reviewer visibility (its own agents-view page) and per-pool exhaustion
  metrics fall out naturally from a dedicated pool.

The automation pool (T793 / #1043) already established the
"second dedicated pool + dispatch routing + per-pool model" pattern. The
review pool is a third pool following the same blueprint, which is why
this alternative is rejected in favor of the _Chosen approach_.

### A4 — Review only on PR _create_, not on update (rejected)

Cheaper: review the first PR a worker opens, but skip review of
subsequent updates (including revisions).

Rejected because:

- The T793 regression class can be _introduced by an update_ (a
  forward-port / conflict-resolution push onto an existing PR), so
  skipping update-review would miss exactly the incident we want to catch.
- However, we keep the _cost_ concern by **skipping no-op / trivial
  updates** (see _No-op skipping_) rather than skipping all updates. That
  gives most of the savings without the blind spot.

_Divergence:_ v1 as first shipped (#1263) reviewed only the initial
implementation push; revision pushes were deferred and landed 2026-07-01
behind `enable_revision_triggered_reviews` (#1712, default **on**).
CI-remediation and conflict-resolution pushes still bypass review today —
see _Known gaps_.

## Chosen approach

A third, dedicated **review worker pool** running always-Opus, fed by a new
`pr_review` execution kind. When a worker's PR is created or updated, the
engine enqueues a `pr_review` execution targeting the review pool. The
reviewer reads the PR read-only, emits structured findings, and — if and
only if the findings clear the engine's severity gate — the engine creates
a **revision task** on the _original producing task_, carrying the
feedback as the revision instructions. The revision is dispatched to the
normal worker pool, updates the PR, and (subject to the cycle bound) is
itself reviewed again. When the reviewer returns no qualifying findings,
or the cycle bound is hit, the PR proceeds to Review.

```
worker opens/updates PR
        │  (finalize_pr_transition in completion.rs)
        ▼
engine enqueues pr_review execution ──► REVIEW POOL (Opus, read-only)
        │                                      │
        │                              structured ReviewResult
        ▼                                      │
   no qualifying findings?  ◄──────────────────┤
        │ yes → PR proceeds to Review          │ severity gate passes
        │                                      ▼
        │                          engine creates REVISION on original task
        │                          (findings digest = revision description,
        │                           created_via = "pr_review:<exec_id>")
        │                                      │
        └──────────────  GENERAL POOL ◄────────┘
                         worker revises, pushes update → loops back to top
                         (bounded by max_review_cycles, tracked on the
                          revision chain root)
```

### 1. Trigger / hook

The trigger lives in **`finalize_pr_transition`** in
`engine/core/src/completion.rs` — the point where the pr_url capture
paths (Stop hook, staged-PR-URL cache, merge-poller sweeps) converge and
a task would otherwise advance to Review.

_Divergence:_ the original plan named `engine/src/pr.rs` / `hooks.rs`;
the capture paths actually converge in `completion.rs`, so the hook went
there. Functionally the same detection point.

On a detected fresh (non-merged) PR for a qualifying producing execution:

1. Run the **no-op / trivial-diff gate** (`check_noop_skip`, see _No-op
   skipping_). If it says skip, log the reason and let the PR proceed.
2. Check the **cycle bound** (see _Loop termination & bounds_). If
   reached, skip review, raise a `pr_review_cycle_bound` attention item,
   and let the PR proceed.
3. Otherwise create a `pr_review` execution
   (`ExecutionKind::PrReview`, status `ready`) carrying the
   `work_item_id` and `repo_remote_url`, kick the scheduler, and complete
   the producing worker with `WorkerPrCompletionTarget::PendingReview` —
   the task's `pr_url` is stamped but its status stays `active`, holding
   it out of the Review column while the review pass runs. On any
   enqueue failure the engine falls back to plain `InReview` so no task
   is ever left stuck.

Qualifying producers (`completion.rs`): `ChoreImplementation` and
`TaskImplementation` completions always; `RevisionImplementation`
completions when `enable_revision_triggered_reviews` is on (default on
since #1712). Reviewer executions themselves never open PRs, so nothing
recurses; `on_stop_inner` / `recheck_for_pr` short-circuit
`ExecutionKind::PrReview`.

_Divergence:_ the plan had the execution row carry `pr_url`, base/head
SHAs, cycle counter, and task context. As built the row carries only
`work_item_id` + `repo_remote_url`; everything else (PR number, base/head
SHA, changed files, diff, last-reviewed SHA) is fetched fresh at
reviewer-spawn time by `fetch_pr_review_context` in `runner.rs`, which
builds a `PrReviewContext`. Fetching at spawn time avoids staleness when
the review sits in the queue.

_Divergence (deliberate, #1279):_ the plan held the producing task out of
human Review until the whole revise→re-review loop resolved. As built,
only the in-flight review pass holds the task (`PendingReview`); once the
pass completes the task advances to `in_review` **even when a revision is
created** — the revision is a follow-up child task on the same PR, and
the parent carries `has_in_progress_revision` for UI badging. This keeps
the Review column honest about "a PR exists and passed at least one
review" at the cost of the stricter "held until the loop converges"
guarantee. See _Known gaps_.

### 2. Reviewer role & prompt

The reviewer worker is spawned with a **review-only** prompt
(`render_reviewer_initial_prompt` in `engine/core/src/pr_review/`) and a
reviewer-specific CLAUDE.md (`render_reviewer_claude_md`) that omits all
PR-creation instructions. Structure:

**Role & hard mandate (read-only):**

> You are an independent PR reviewer. Your ONLY job is to review the diff
> and return structured feedback. You MUST NOT change the PR in any way:
> no commits, no pushes, no `gh` writes, no edits to the branch or any
> file, no comments on GitHub. You operate read-only. Anything you would
> "fix" you instead describe as a finding. Posting to GitHub is
> prohibited — your feedback stays inside Boss.

This mandate is reinforced at the tooling level (see _Read-only
enforcement_).

**What to review (inputs):** the PR diff against the merge base, the
producing task's stated purpose, and the changed files in context. The
reviewer workspace is checked out to the PR head SHA so the reviewer
reads files directly. The prompt carries the pre-fetched
`PrReviewContext`: PR number, base/head SHA, changed files, diff content,
`last_reviewed_sha`, supersession flags, merged-parent deletion signals,
and Boss-construct references.

**High-bar, actionable rubric.** Push back, with a HIGH bar and VERY
actionable feedback, on:

- **Critical correctness issues / bugs** — logic errors, broken
  invariants, mishandled errors, race conditions.
- **Inadvertent deletions / regressions** _(first-class, explicit check)_
  — diff against the merge base and flag anything dropped that is
  unrelated to the PR's stated purpose. Conflict-resolution and
  forward-port PRs get extra scrutiny here. This is the T793 check: a live
  feature silently removed during a forward-port must be caught.
- **Architectural improvements** — wrong layer, missed reuse, abstraction
  that fights the codebase's conventions.
- **Code quality & readability** — match surrounding style; naming;
  dead/confusing code.
- **Test coverage gaps** — untested new behavior, missing edge-case tests.
- **Edge cases & gotchas** — boundary conditions, nullability, concurrency,
  failure modes.

As built the rubric also grew three operator-directed categories beyond
the original six — **duplication** (reimplementing something the repo
already has), **deferred scope** (silently dropping promised scope), and
**agent-isms** (LLM-artifact prose/comments) — all of which force a
revision regardless of severity (see the severity gate below).

**Actionability requirement:** every finding must name a file (and line/
hunk where possible) and state concretely what to change. "Consider
improving error handling" is not acceptable; "in `pr.rs` the `?` on the
gh call swallows the 422 — handle the duplicate-PR case explicitly" is.

**Speed / comprehensiveness balance (explicit in prompt):**

> Prefer fast, high-signal feedback over exhaustive analysis. Every PR may
> now pass through up to ~3 produce→review→revise cycles, so do NOT
> excessively lengthen turnaround. If in doubt about a non-critical
> suggestion, you MAY offer it WITHOUT deep analysis and mark it
> low-severity — the downstream revision worker decides whether to apply
> it. Spend your scrutiny budget on correctness and regressions first.

### 3. Structured reviewer output (`ReviewResult`)

The reviewer emits a single structured `ReviewResult`
(`engine/core/src/pr_review/types.rs`, `bon::Builder` structs):

```jsonc
{
  "pr_url": "https://github.com/...",
  "head_sha": "abc123",
  "summary": "one-paragraph overall read",
  "revision_warranted": true, // advisory — the engine gate governs
  "findings": [
    {
      "severity": "critical | high | medium | low",
      "category": "correctness | regression | architecture | readability | tests | edgecase | duplication | deferred_scope | agent_isms",
      "file": "tools/boss/engine/core/src/pr.rs",
      "location": "fn ensure_pr, ~L120", // optional, best-effort
      "title": "Forward-port dropped the autostart feature",
      "detail": "Concrete description + what to change.",
      "confidence": "high | medium | low", // low = suggestion, apply at revisor's discretion
    },
  ],
  "regression_check": {
    "performed": true,
    "suspected_deletions": [], // derived by the engine from category=regression findings
  },
}
```

- `revision_warranted` is computed by the reviewer but **gated by the
  engine** (`passes_severity_gate` in `pr_review/parsing.rs`): a revision
  is created iff at least one finding is `critical`/`high`, **or** any
  finding has category `regression`, `duplication`, `deferred_scope`, or
  `agent_isms` (the latter three added post-design by operator directive
  — they mark defects severity alone under-weights). The gate governs in
  both directions: a `revision_warranted: true` with only medium
  readability findings creates no revision.
- _Divergence (robustness):_ the plan made `regression_check` mandatory
  ("must be present, `performed` must be true"). In practice reviewers
  sometimes omitted or malformed it and strict parsing discarded whole
  reviews, so as built `regression_check` is `#[serde(default)]`
  (T1359) and `suspected_deletions` is derived by the engine from
  `category == regression` findings rather than trusted from the reviewer
  (T1687). `performed: true` remains prompt-enforced only.

**Delivery / enforcement.** As first shipped (#1239/#1279) the result was
scraped from a ` ```json ` fenced block in the reviewer's final
transcript message — free text, silently tolerant of a rambling reviewer.
As built today the reviewer **writes the JSON to an engine-owned artifact
path** (`$BOSS_STRUCTURED_OUTPUT`, the reviewer's one sanctioned Write),
which the engine reads and schema-validates at the Stop boundary with a
bounded re-prompt loop on missing/invalid output
(`StopOutcome::ReviewPassAwaitingResult`, nudge key
`"pr_review:awaiting_result"`, give-up attention after the bound); the
transcript scrape survives only as a transitional fallback for
remote/SSH reviewers. This closed the plan's "enforced via the
structured-output mechanism, not free text" requirement, two hardening
steps after v1.

### 4. Feedback → revision (internal, never GitHub)

When the severity gate passes, `finalize_pr_review_pass` creates a
**revision task** on the **original producing task**:

- `parent_task_id` = the producing task.
- The revision `description` = `render_revision_instructions(result)` — a
  digest of the qualifying findings sorted critical→low
  (`### [severity] title`, file/location, category, confidence, detail,
  then the review summary). This is the _internal_ feedback; it lives in
  Boss only.
- Provenance: `created_via = "pr_review:<review_execution_id>"`
  (`CREATED_VIA_PR_REVIEW_PREFIX` in `protocol/src/types/common.rs`),
  extending the existing `ci_fix:` / `merge_conflict:` remediation-source
  taxonomy.
- The revision dispatches on the **general** worker pool with
  `autostart = true` (revising is production work).

_Divergence:_ the plan had the revision "explicitly inherit" the parent's
`pr_url` as T955 belt-and-suspenders. As built, revision rows carry
`pr_url = NULL` by design and the PR is resolved through the revision
chain root (`WorkDb::get_revision_chain_root_pr_url`, plus the
`StagedRevisionPushCache` "PR update satisfies PR-exists" machinery from
the revision-tasks work). The T955 strand-avoidance _outcome_ is achieved
by that pre-existing machinery; no separate T955 fix was needed in this
project.

The revision resumes the branch, applies the feedback, and pushes an
update → which re-enters `finalize_pr_transition` → which (subject to the
cycle bound and no-op gate, and to `enable_revision_triggered_reviews`)
enqueues another `pr_review`. The loop converges when the reviewer
returns no qualifying findings or the bound is hit.

If no revision is warranted, nothing is created and the PR proceeds to
Review. The reviewer never writes to GitHub in either case — the only
external call in the whole path is the pre-existing PR-open check inside
`create_revision`.

### 5. Review worker pool + execution kind + dispatch routing

Modeled directly on the automation pool (T793 / #1043), as the third pool
following the same blueprint. As built (`engine/core/src/coordinator.rs`):

- **Execution kind:** `ExecutionKind::PrReview`, wire/DB string
  `"pr_review"` (`protocol/src/types/execution.rs`; originally shipped as
  the string constant `EXECUTION_KIND_PR_REVIEW` in #1199 — the kinds
  were stringly-typed then and became an enum later; the constant remains
  as a back-compat alias).
- **Pool construction:** `WorkerPool::new_review(size)` with worker-id
  prefix `review-` (`REVIEW_WORKER_ID_PREFIX`), a `review_pool` slot on
  `ExecutionCoordinator` with `set_review_pool` /
  `review_worker_pool()`, and a third disjoint pane-slot range.
- **Routing:** `execution_targets_review_pool(&WorkExecution)` (kind ==
  `PrReview`); `pool_for_execution` checks review **first**, then
  automation, then main — so a review of an automation-produced task
  still lands on the review pool. `pool_for_worker_id` routes the
  `review-` prefix, so `release_worker_and_kick` and the dead-pid/stale
  release paths work unchanged. `drain_ready_queue` has a third
  per-pool exhaustion flag (`review_pool_exhausted`) so a full review
  pool never blocks main/automation dispatch or vice versa.
- **Per-pool always-Opus model override:**
  `pool_model_override_for_worker_id(worker_id)` returns `"opus"` for
  the `review-` (and `auto-worker-`) prefixes; `resolve_spawn_config`
  consults it with precedence: task `model_override` → pool override →
  effort-level default → product default → engine default.
  _Divergence:_ the plan claimed the automation pool already had such an
  override to reuse. It did not — automation was Opus only incidentally
  (triage carries no effort level, so it fell through to the engine
  default). #1234 **created** the mechanism and pinned both pools,
  fixing the automation pool's latent gap in the same PR. The override is
  keyed off the worker-id prefix in a free function rather than stored on
  the pool object — functionally equivalent, but a fourth pool would need
  a code change there, not config.
- **Pool sizing:** `review_pool_size` config, env `BOSS_REVIEW_POOL_SIZE`.
  _Divergence (deliberate reversal, #1299):_ the plan said "default small
  (1–2) to bound concurrent Opus spend" and v1 shipped default 2 / cap 3.
  Review-queue contention in practice led to raising both default and cap
  to **8** (matching the main pool) — latency won over the small-pool
  cost bound, which is now effectively vacuous. Post-ship, the
  hand-offset three-pool slot arithmetic also proved fragile (reviewer
  spawn failures) and was consolidated into a single slot-range source of
  truth (#1320), with spawn-failure recovery hardening in #1329 /
  `pr_review_recovery.rs`.

### 6. Agents-view page (macOS app)

As built (#1233): the agents view has a **Reviewers** pool tab
(`AgentPoolKind.reviewers`, label `"Reviewers (N)"`) with its own
`WorkerGrid` of terminal panes over `reviewSlots`, mirroring the
automation-pool page exactly. Slot counts are synced live from the
engine's `EnginePoolConfig` push (the original hardcoded mirror of the
pool size was replaced when the pool grew to 8). All pane operations
(spawn/release/send/focus/interrupt) route review-range slots correctly.

_Divergence:_ the plan promised review-specific columns — current
PR-under-review, cycle number, recent verdicts, and skip reasons. None of
these shipped; the page is a bare pane grid (like the automation page it
mirrors), and a reviewer's current PR is visible only indirectly via the
pane title. Nothing downstream persists `ReviewResult`s to feed such
columns. This is the largest unfinished piece of the design — see _Known
gaps_. Review outcomes are instead visible indirectly: qualifying
findings become revision tasks on the normal boards, and cycle-bound
exits become attention items.

### 7. Loop termination & bounds

The produce→review→revise loop terminates via (first to fire wins):

1. **Reviewer approves** — no findings clear the severity gate → no
   revision → PR proceeds. (Normal, desired exit.)
2. **Max review cycles per PR** — `max_review_cycles` (config, env
   `BOSS_MAX_REVIEW_CYCLES`, default `DEFAULT_MAX_REVIEW_CYCLES = 3`).
   `finalize_pr_review_pass` increments `tasks.review_cycle` (and stamps
   `tasks.last_reviewed_sha` from the reviewer-reported head SHA) after
   every completed pass; `finalize_pr_transition` skips enqueueing when
   the counter has reached the bound, raises a sticky attention item
   (`kind: "pr_review_cycle_bound"`), and lets the PR proceed to human
   Review. Bound-state reads fail open.
3. **Convergence** — successive reviews whose findings all fall below the
   gate create no revision (case 1 applied each cycle).

_Divergence (bug, fixed post-ship):_ v1 tracked `review_cycle` /
`last_reviewed_sha` on the execution's own task row. For
revision-triggered reviews that row is the fresh revision task — always
cycle 0 — so the bound (and the no-op gate's SHA comparison) silently
reset every revision and could never trip on exactly the path they exist
to bound. #1712 fixed this: both counters now live on the **revision
chain root** (`WorkDb::review_cycle_root_id`).

_Divergence:_ the plan put three knobs in config — `max_review_cycles`,
the severity gate, and per-cycle gate decay (stub, default off). Only
`max_review_cycles` is configurable; the gate is hardcoded in
`passes_severity_gate` and no decay stub exists. The plan also promised
the last unresolved findings would be attached internally when the bound
is hit; as built the attention item is boilerplate pointing at "the most
recent revision task", and no findings digest is stored. See _Known
gaps_.

Schema: `tasks.review_cycle INTEGER NOT NULL DEFAULT 0` and
`tasks.last_reviewed_sha TEXT` (schema v19). The production `map_task`
deliberately does not select these columns; targeted reads go through
`get_task_review_cycle_state`.

### 8. No-op skipping

Before enqueueing a `pr_review`, `check_noop_skip` (`completion.rs`)
runs, in order:

1. **First review is never skipped** — no `last_reviewed_sha` or
   `review_cycle == 0` → always review.
2. **Head unchanged** — live head OID (fetched via
   `BranchVerifier::fetch_pr_head_oid`, not trusted from the stored SHA)
   equals `last_reviewed_sha` → skip (`sha_unchanged`).
3. **Pure rebase / empty effective diff** — the
   `last_reviewed_sha...head` compare (GitHub compare API,
   `fetch_diff_line_count`) totals 0 changed lines → skip
   (`empty_diff`).
4. **Trivial diff** — below `min_review_changed_lines` (config, env
   `BOSS_MIN_REVIEW_CHANGED_LINES`, default **0** = disabled) → skip
   (`trivial_diff`). The conservative default means only literal no-ops
   are ever skipped — a one-line correctness fix is always reviewed.

Every uncertainty fails open (unparsable PR URL, head-fetch or compare
error → review anyway): uncertainty never silently suppresses a reviewer
pass. The gate runs **before** the cycle-bound check, so a pure rebase
consumes no cycle slot and raises no cycle-limit attention.

_Divergence:_ skips are logged (`tracing::info!` with a `skip_reason`
field) but never persisted or surfaced in the UI — the plan's "surfaced
on the Reviewers page so 'why wasn't this reviewed' is answerable" did
not ship (see _Known gaps_). The plan's "whitespace-only" trivial rule is
also not implemented: line counts come from the compare API, which counts
whitespace changes, so with the default threshold a whitespace-only push
is reviewed (over-review, the safe direction).

### 9. Read-only enforcement

Three layers as built (`engine/core/src/worker_setup.rs`):

1. **Worker kind.** A `WorkerKind` enum (`Standard | Reviewer | Triage |
AnswerAgent`) selects per-kind spawn behavior.
   `worker_kind_for_execution` maps `ExecutionKind::PrReview →
WorkerKind::Reviewer` with no wildcard arm — adding a new execution
   kind forces a compile error — and is called from both the local and
   remote spawn paths, so the reviewer posture cannot be forgotten at a
   spawn site. (The plan's open question "does the spawn path support
   per-kind denies?" was answered **no** by #1235, which built the
   capability.)
2. **Tool denylist** (the enforceable contract): `reviewer_deny_rules`
   denies `Edit({workspace_parent}/**)` — fenced to the workspace tree
   rather than `Edit(**)`, which permits the reviewer's one sanctioned
   write: the `ReviewResult` JSON artifact in the engine-owned `$TMPDIR`
   path (a parallel `Write(**)` rule is dead weight because the harness
   matches Write against `Edit(path)` rules too) — plus the shared
   `publish_deny_rules`: `jj git push`, `git push`,
   `gh pr create/merge/close/edit/comment/review`,
   `gh issue create/comment/close/edit`, and all of `cube pr`, each in
   bare and `:*` forms. Local-only VCS (`jj describe`,
   `jj bookmark create`) stays allowed.
3. **Prompt mandate**: the reviewer CLAUDE.md and initial prompt state
   the read-only / no-GitHub mandate prominently, and the reviewer
   CLAUDE.md omits PR-creation instructions entirely.

The reviewer is dispatched with a real cube workspace checked out to the
PR head SHA before spawn. _Divergence:_ the checkout uses `jj new <sha>`,
not the planned `jj edit <sha>` — a pushed PR head is immutable in jj, so
`jj edit` fails deterministically.

_Known hole:_ `gh api` (and a few other `gh` write surfaces like
`gh pr ready`, `gh repo edit`, `gh release create`) are not in the
denylist, so the "no `gh` write subcommands" guarantee is enumerated, not
complete — a reviewer could mutate GitHub via `gh api -X POST`. See
_Known gaps_.

### 10. Cost & latency analysis

**Per-PR added cost.** One Opus review per qualifying PR update. With the
no-op gate, the _number_ of reviews ≈ number of meaningful pushes, not
every push. Worst case per PR ≈ `max_review_cycles` (default 3) Opus
reviews + up to 2 extra revision turns by the general pool.

**Latency.** Each cycle adds: review-pool queue wait + one Opus review
turn. In practice queue wait, not review depth, was the binding
constraint — which is why the pool grew from 2 to 8 slots (#1299).

**Controls that bound cost** (as built):

- `max_review_cycles` (default 3) caps cycles per PR, tracked on the
  revision chain root.
- No-op / trivial-diff gate avoids re-reviewing rebases and no-ops.
- Severity gate avoids spawning revisions (and thus further reviews) for
  low-severity nits.
- Always-Opus is scoped to the review pool; production effort/model
  selection is unchanged.
- ~~Review-pool slot count caps concurrent Opus spend~~ — _Divergence:_
  with the pool at 8 (equal to the main pool) this control is no longer
  meaningfully bounding; the operator traded it for review latency. No
  per-day budget circuit breaker exists (explicitly deferred).

**Expected steady state.** Most PRs: 1 review, 0–1 revision. Problem PRs:
up to 3 reviews. This matches "raise quality without excessively
lengthening turnaround."

### 11. Interaction with other work

- **T955 (revision nudge-loop).** Resolved without a dedicated fix in
  this project: revision `pr_url` is NULL by design and PR resolution
  goes through the revision chain root + staged-revision-push machinery
  from the revision-tasks work, which already treats a PR _update_ as
  satisfying "PR exists". Review-generated revisions do not strand.
- **P576 / editorial controls.** The reviewer never talks to GitHub, and
  is denied GitHub-write tools entirely (section 9), so there is no
  GitHub-facing action for the editorial evaluator to police. The
  revision-instructions text is internal and never flows to an
  editorial-evaluated surface.
- **unify-pr-remediation-on-revisions.** The reviewer is a remediation
  _source_ expressed as the `created_via` prefix `"pr_review:"`,
  alongside the existing `ci_fix:` / `merge_conflict:` prefixes — it
  slots into that taxonomy rather than inventing a parallel path.
- **Incident-002 deletion tripwire.** Post-ship, `finalize_pr_review_pass`
  gained a merged-parent deletion-signoff tripwire, which partially
  covers the T793 class for pushes that bypass the reviewer (see _Known
  gaps_ on CI-fix / conflict-resolution coverage).

### Which PRs are in scope

- **In scope:** agent-authored code PRs from primary implementation
  executions (`ChoreImplementation`, `TaskImplementation`) on create,
  and revision pushes when `enable_revision_triggered_reviews` is on
  (default on since 2026-07-01, env
  `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS`). Note this includes
  human-initiated revisions, broader than the original
  reviewer-created-revisions-only plan.
- **Light scope:** docs-only PRs get a light rubric — structure,
  completeness, internal consistency, required-sections check — instead
  of the code rubric. As built the engine classifies the changed-file
  list at spawn time (`classify_changed_files` → `ReviewScope::{Code,
DocsOnly}`; docs = `.md`/`.mdx`/`.rst`/`.txt` or `docs/` paths),
  failing open to `Code` with an in-prompt self-detection fallback.
  (v1 of #1259 shipped the classifier unwired — always `Code` — and was
  wired in a follow-up.)
- **Out of scope:** human-authored PRs; the reviewer's own output
  (reviewers produce no PRs, so nothing recurses); and — _not by
  design_ — CI-remediation and conflict-resolution pushes, which
  currently bypass the reviewer (see _Known gaps_).

## Known gaps (as of 2026-07-20)

Gaps between this design and current `main`, surfaced by the design
postmortem; each is being scheduled as follow-up work:

1. **Reviewers page has no review columns and no verdict/skip surfacing**
   (§6, §8). The page is a bare pane grid; no `ReviewResult` is persisted
   anywhere (a clean review's findings are discarded; only gated reviews
   survive as revision text), no cycle number or skip reason reaches the
   app. "Why wasn't this reviewed" is answerable only from engine logs.
2. **`gh api` not denied for reviewers** (§9). The publish denylist
   enumerates `gh pr`/`gh issue` subcommands only; `gh api` can mutate
   GitHub.
3. **Cycle-bound exit does not attach unresolved findings** (§7). The
   attention item is a fixed pointer, and because the bound-hit skips the
   review there is no statement of what actually remains unresolved.
4. **Severity gate and decay have no config presence** (§7). Only
   `max_review_cycles` is tunable; the gate is hardcoded.
5. **CI-remediation and conflict-resolution pushes bypass review** (§1,
   A4). The conflict-resolution push is exactly the T793 class; today it
   is reviewed only if it arrives via a revision execution (the
   incident-002 deletion tripwire partially mitigates).
6. **Real-PR calibration never happened** (task 13). #1308's e2e tests
   drive the completion state machine with synthetic `ReviewResult`
   fixtures; no reviewer was ever validated against a real T793-style
   forward-port diff.

## Risks / open questions

1. **Mandatory-pass vs. throughput.** _Resolved in practice:_ only the
   in-flight review pass blocks (task held `active` via `PendingReview`);
   revision cycles run with the parent already in Review, and the pool
   was grown to 8 slots when queue wait dominated. The stricter
   "held until the loop converges" semantics were consciously dropped
   (#1279).
2. **Severity gate calibration.** Defaults shipped as designed and were
   then widened (duplication / deferred-scope / agent-isms force
   revisions). Still hardcoded — tuning requires a code change (gap 4).
3. **No-op gate aggressiveness.** _Resolved:_ shipped maximally
   conservative (`min_review_changed_lines = 0`; only literal no-ops
   skipped), fail-open on every uncertainty.
4. **Per-worker-kind tool denylist.** _Resolved:_ the capability did not
   exist and was built (#1235, `WorkerKind` + per-kind deny rules), with
   exhaustive kind mapping added later. Residual: the `gh api` hole
   (gap 2).
5. **Cost ceiling.** Open, and sharper now that the pool sits at 8
   always-Opus slots: no per-day review-budget circuit breaker exists,
   and the small-pool bound is gone. Cycle caps and the no-op gate are
   the remaining controls.
6. **Cycle-bound exit UX.** Partially resolved: a sticky attention item
   fires on the work item. The findings themselves are not attached
   (gap 3), and nothing shows on the Reviewers page (gap 1).
7. **Reviewer reading the diff.** _Resolved:_ workspace checked out to
   the PR head (`jj new <sha>`); prompt carries the pre-fetched diff and
   changed-file list; `gh pr diff` available as backup.
8. **Interaction with stacked PRs.** Still open; unchanged (explicitly
   deferred).

## Implementation history

Planned task → shipped PR mapping (all merged 2026-06-01 → 2026-06-03):

| #   | Task                             | PR    | Notes                                                                            |
| --- | -------------------------------- | ----- | -------------------------------------------------------------------------------- |
| 1   | Per-worker-kind tool denylist    | #1235 | Capability did not exist; built `WorkerKind` + `reviewer_deny_rules`             |
| 2   | T955 fix verification            | —     | No dedicated change needed; covered by revision chain-root machinery             |
| 3   | `pr_review` execution kind       | #1199 | Shipped as string constant; later became `ExecutionKind::PrReview` enum variant  |
| 4   | Review worker pool + routing     | #1209 | Default size 2/cap 3 at ship; raised to 8/8 by #1299                             |
| 5   | Always-Opus pool override        | #1234 | Mechanism created (didn't pre-exist); also pinned the automation pool            |
| 6   | Reviewer prompt + `ReviewResult` | #1239 | Fenced-JSON transcript scrape at ship; artifact-path enforcement came later      |
| 7   | Trigger from pr_url capture      | #1263 | In `completion.rs::finalize_pr_transition`; primary implementations only at ship |
| 8   | Feedback → revision wiring       | #1279 | Parent advances to Review even with a revision open (deliberate)                 |
| 9   | Loop termination & bounds        | #1296 | Cycle tracked on task row at ship — reset bug on revisions, fixed by #1712       |
| 10  | No-op / trivial-diff skip gate   | #1302 | Log-only skip reasons                                                            |
| 11  | Reviewers page (macOS)           | #1233 | Pane grid only; planned PR/cycle/verdict columns never shipped (gap 1)           |
| 12  | Docs-only light rubric           | #1259 | Classifier shipped unwired (always `Code`); engine-side wiring came later        |
| 13  | End-to-end test + tuning         | #1308 | Synthetic-fixture state-machine tests; no real-PR calibration (gap 6)            |

Notable post-ship PRs: #1299 (review pool 2→8), #1320 (slot-range source
of truth after reviewer spawn failures), #1329 + `pr_review_recovery.rs`
(spawn-failure / orphaned-review hardening), #1712 (revision-triggered
reviews behind `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS`, chain-root
cycle tracking).

### Deferred / future (not a v1 blocker)

- **Configurable review-rubric policy layer** — move the baked-in rubric to
  a configurable policy (mirroring the `editorial.rs` → editorial-controls
  evolution).
- **Per-cycle severity-gate decay** — auto-raise the gate on later cycles
  so late cycles only block on critical issues.
- **Hard per-day review-budget circuit breaker** — global cost cap; more
  relevant now that the pool is 8 always-Opus slots.
- **Stacked-PR per-entry review semantics** — review each PR in a stack
  against its stack parent.
- **Non-blocking later cycles** — moot in current form: only the single
  in-flight review pass blocks (see §1); revisit if the hold semantics
  are ever tightened back toward the original plan.
