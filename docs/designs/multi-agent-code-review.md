# Multi-agent code review: independent reports, one non-verifying supervisor

- Date: 2026-09-01
- Status: proposed — design only, no implementation
- Provenance: project design for Multi-agent code review
- Related designs: [automated reviewer pass](../../tools/boss/docs/designs/automated-reviewer-pass-on-every-agent-authored-pr.md), [worker proposal API](../../tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md), [revision tasks](../../tools/boss/docs/designs/revision-tasks.md), [unified PR remediation](../../tools/boss/docs/designs/unify-pr-remediation-on-revisions.md)

The contested property is explicit: three reviewers remain independent until a fourth agent collates their structured reports, and that supervisor does not inspect source or re-run the review. This design accepts a small sequential collation step in exchange for provider diversity, per-reviewer failure isolation, and one durable outcome.

## Verdict

Replace each pre-merge reviewer pass with a persisted review batch containing three parallel, read-only executions pinned to Claude, Codex, and Grok, followed by a cheap Claude Sonnet supervisor. Select each leaf model from the PR's own recorded size-and-complexity profile, submit both reports and the consolidated verdict through `boss propose`, and create either an ordinary revision on an open PR or a follow-up against `main` after merge.

Use a static 16-slot review pool with batch-aware admission. A pre-merge batch reserves four slots, so four PRs can progress concurrently without leaf reviews starving supervisors; post-merge reviews consume one slot and remain lower priority than pre-merge work.

## Goals

- Run three genuinely independent pre-merge reviews in parallel: one each on the Claude, Codex, and Grok drivers.
- Keep leaf reviews cheap and bounded at provider effort `medium`, while varying the model from the PR's own size and complexity rather than the parent work item's effort.
- Produce one consolidated, attributable verdict without majority-vote suppression of unique findings.
- Continue the existing review/revise loop, counting one completed batch—not three leaf executions—as one review cycle.
- Trigger a deeper review of the landed code for large or complex production PRs and turn qualifying findings into an automatically dispatched follow-up against `main`.
- Enforce a static-analysis-only reviewer posture: no edits, pushes, GitHub writes, builds, tests, formatters, generators, or execution of changed code.
- Move reviewer findings delivery from completion-time artifact/transcript parsing to the typed, validated, idempotent `boss propose` channel.
- Preserve the current user-facing AI-review state and revision/follow-up provenance while adding enough batch detail to diagnose provider failures and model-selection decisions.

## Non-goals

- Dynamic pool sizing or changes to interactive and automation pool behavior.
- Fixing host sleep. The separate wake-assertion work is a prerequisite for realizing the latency benefit, not part of this project.
- Changing the work-item `--effort` classifier or using work-item effort as a review-size proxy.
- Posting leaf reports, supervisor discussion, or findings to GitHub.
- Letting the supervisor become a fourth source reviewer.
- Giving review agents permission to build or run tests in exceptional cases.
- Replacing human PR review or making a clean automated verdict sufficient to merge.
- Running the three-agent pre-merge topology again after merge. Post-merge review is a distinct, single-agent integration safety net.

## Current state and constraints

### One reviewer, one driver, one strong model

Every current `pr_review` execution is routed by the review-pool policy to Claude at the strong tier, which resolves to Opus. The reviewed row's own driver is intentionally ignored, and the owning work item's effort is passed through to the reviewer. This means there is neither driver diversity nor a PR-derived model signal today; the apparent effort relationship is confounded by larger work items tending to produce larger PRs.

The current dispatch and model resolution are centralized in [`coordinator.rs`](../../tools/boss/engine/core/src/coordinator.rs) and [`worker_spawn.rs`](../../tools/boss/engine/core/src/runner/worker_spawn.rs). That centralization is reusable, but review and automation currently share the same fixed pool policy, so this project must specialize review policy without changing automation.

### Output bypasses `boss propose`

The reviewer writes a `ReviewResult` JSON artifact and repeats it in its transcript as a fallback. [`finalize_pr_review_pass`](../../tools/boss/engine/core/src/completion/finalize_passes.rs) parses that output, records one `pr_review_verdicts` row, applies the severity gate, and directly calls `create_revision`.

`boss propose` is already a synchronous worker-to-engine RPC with peer-derived execution attribution, typed per-kind validation, idempotency keys, rate caps, a durable `worker_proposals` ledger, and auto-applied versus gated policies. Workers currently use its closed vocabulary for attention, effort escalation, blocked state, deferred scope, follow-up suggestions, automation outcomes, and PR-created declarations; the engine either applies the declaration immediately or leaves a durable proposal for the owning workflow. There is no review kind today. The earlier proposal design deliberately deferred reviewer migration because the artifact was healthier than transcript-only seams; this project now takes on that explicit deferred seam.

### Read-only is enforced, but “do not build” is not

The request's uncertainty is partly resolved in the existing implementation:

- `ExecutionKind::PrReview` maps exhaustively to `WorkerKind::Reviewer` in [`worker_setup.rs`](../../tools/boss/engine/core/src/worker_setup.rs).
- Reviewer prompts and Claude deny rules block workspace edits, pushes, PR/issue writes, and Boss PR helpers.
- Codex reviewer executions always use the OS-enforced `read-only` sandbox in [`codex.rs`](../../tools/boss/engine/driver/src/codex.rs).
- Grok uses a read-only sandbox off-host, plus workspace edit denies; local macOS Grok deliberately runs its sandbox off so repository test sandboxes can work.
- The Codex tool-surface guard closes unobservable stdin-driven command channels and app/MCP calls, but it is not a build-tool guard.

These controls establish “cannot change or publish the PR.” They do not establish “cannot invoke a build or test”: Claude may run one through Bash, Codex may attempt one against external caches, and local Grok explicitly retains the capability needed by Bazel. The design therefore adds a reviewer-only build/execution command guard across all three drivers rather than claiming the existing write fence already meets the requirement.

### Current orchestration assumes one live reviewer

The enqueue dedup, dead-review recovery, and chain single-writer guard all assume at most one non-terminal `pr_review` execution for a work item. In particular, the chain guard treats review-versus-review as a hold. Three rows cannot simply be inserted and expected to run in parallel; batch membership must become the key for dedup, recovery, and the read-only concurrency exception.

### Existing remediation behavior is reusable but incomplete at the merge race

For findings produced while the origin PR is open, the engine already creates an autostart revision that commits to the existing branch. If that revision is still pending when the origin PR merges, the shared parent-close resolver converts review-findings work in place to a `followup` row with origin PR provenance, and its implementation opens a new PR against `main`.

However, if the PR merges before `finalize_pr_review_pass` calls `create_revision`, the create-time open-PR gate rejects the revision and the current finalizer records `revision_creation_failed`; no follow-up is materialized. The consolidated-verdict applier must unify these paths so a merged origin makes a finding a follow-up, never moot and never discarded.

### Measured performance baseline

The supplied baseline covers 434 executions, including 52 deeply parsed transcripts. It shows near-constant throughput of about 3,600 output tokens per minute and the following effort medians:

| Provider effort | Awake wall time | Turns | Output tokens | Context peak |
| --------------- | --------------: | ----: | ------------: | -----------: |
| low             |         1.1 min |     6 |         4,531 |          58k |
| medium          |         2.6 min |    12 |         8,226 |          72k |
| high            |         5.9 min |    21 |        26,382 |         107k |
| xhigh           |         7.6 min |    25 |        34,380 |         137k |

Three medium leaves therefore budget 24,678 output tokens and about 2.6 minutes of parallel awake time. The supervisor contract below targets no more than 4,000 output tokens, adding about 1.1 minutes at the measured throughput: approximately 28,678 output tokens and 3.7 minutes end to end, versus 34,380 tokens and 7.6 minutes for one xhigh reviewer.

The output-token advantage holds while the supervisor stays below 9,702 tokens; the awake-wall advantage holds while it stays below about five minutes. The 4,000-token target leaves meaningful margin on both. This is a planning envelope, not a measured supervisor result, and it excludes provider price differences and input-token cost; rollout telemetry must validate it.

Host sleep currently consumed 38% of measured review wall time, and 95% of gaps longer than 120 seconds were attributable to sleep. Three concurrent reviewers suspend together, so concurrency does not recover that lost wall time. The separate wake assertion must land before this project evaluates its latency claim; this design neither duplicates nor works around that prerequisite.

## Chosen approach

### Persist one immutable review batch per target

Introduce a review-batch model rather than encoding roles in `created_via` or inferring siblings from timestamps.

A batch records:

- identity: batch id, cycle-root work item, PR URL and number;
- phase: `pre_merge` or `post_merge`;
- immutable target: base SHA, reviewed head SHA, and merge SHA when applicable;
- the complete classification input and selected profile;
- lifecycle: collecting, supervising, applying, completed, or failed;
- timestamps and the final verdict/proposal id.

Batch members record batch id, role, execution id, requested driver, resolved model, provider effort, attempt number, and terminal/report state. Pre-merge roles are `claude_reviewer`, `codex_reviewer`, `grok_reviewer`, and `supervisor`; post-merge uses `post_merge_reviewer`. The unique key `(batch_id, role, attempt)` makes retries explicit, while `(cycle_root_id, phase, target_sha)` prevents two batches for the same immutable target.

Resolved model and effort belong to the member, not the task. The scheduler reads the member policy at spawn, so review configuration cannot inherit `tasks.driver`, `tasks.model_override`, `tasks.reasoning`, or `tasks.effort_level`. The batch preserves both classifier inputs and the resolved model names, making a later menu or threshold change unable to rewrite history.

### Compute size and complexity once, before dispatch

The engine computes a `ReviewProfile` when it first creates the batch, using the PR's own GitHub file metadata. It performs this before member executions exist, persists the result, and reuses that same snapshot for all three leaves, the supervisor, re-review-cycle accounting, and post-merge eligibility.

Inputs are:

- additions plus deletions across the PR;
- changed-file count;
- distinct path-based subsystem buckets;
- production-language bucket count;
- whether all changed files are docs/test fixtures versus any production code;
- named complexity flags derived from paths: database/schema migrations, authentication/permissions/sandboxing, scheduler/concurrency/process lifecycle, and build/release/dependency surfaces.

Subsystem bucketing is deliberately lexical, not a claim of architectural ownership: paths below `tools/<product>/...` use their first three components, other nested paths use their first two directory components, and repository-root files share a `root` bucket. Language buckets group source extensions into Rust, Swift, Starlark, shell, web, and other production code; docs, generated snapshots, and fixtures do not add production-language buckets. All raw paths and counts remain on the batch for audit.

The initial policy is deterministic:

- **Light:** at most 200 changed lines, five files, one subsystem, and one production-language bucket, with no complexity flag. Docs-only or test-only PRs may use the relaxed limits of 400 lines and ten files, still within one subsystem and with no complexity flag.
- **Deep:** more than 1,000 changed lines, more than 25 files, at least four subsystems, at least three production-language buckets, or at least two complexity flags.
- **Standard:** everything else. One complexity flag forces at least Standard even when the diff fits Light's numeric bounds.

Missing or incomplete GitHub metadata fails conservatively to Standard and records the missing fields. It does not inherit work-item effort and does not silently classify as Light.

Only a Deep batch that contains production code is eligible for post-merge review. Large docs-only and test-only PRs still receive an appropriately profiled pre-merge review, but do not trigger a landed-code integration pass.

### Map profiles through each driver's real model menu

Extend the driver model menu with a review-specific `fast`, `balanced`, and `strong` mapping. The policy is concrete at current HEAD:

| Review profile | Claude   | Codex           | Grok       | Provider effort |
| -------------- | -------- | --------------- | ---------- | --------------- |
| Light          | `sonnet` | `gpt-5.6-luna`  | `grok-4.6` | `medium`        |
| Standard       | `sonnet` | `gpt-5.6-terra` | `grok-4.6` | `medium`        |
| Deep           | `opus`   | `gpt-5.6-sol`   | `grok-4.6` | `medium`        |

The Claude mapping follows the requested example: small/simple work uses Sonnet, while large/complex work earns Opus. Codex's corresponding progression is Luna → Terra → Sol.

Grok has no equivalent fast tier in the authenticated menu recorded by the driver: it exposes current `grok-4.6` and retained `grok-4.5`, while the former fast-code model is retired and silently redirects. Selecting an old generation merely to manufacture variability would be an unverified downgrade, so all three profiles use `grok-4.6`. This is an explicit capability limit; a future active fast model can populate the menu mapping without changing the classifier or batch schema.

Every leaf receives provider effort `medium`. Model capability varies with the PR; effort does not vary with the parent task, which removes the current confound and keeps the measured comparison interpretable.

### Dispatch three executions, not one execution with subagents

The batch reconciler atomically inserts one member and one `pr_review` execution per driver, then kicks the scheduler. Each execution gets its own provider process, lease, transcript, proposal attribution, retry state, and review-pool slot.

The per-PR chain guard gains one narrow exception: read-only leaf members in the same batch may run concurrently with one another. They still block every writer, a reviewer from another batch for the same target cannot exist because of the batch unique key, and the existing conflict-resolution preemption remains unchanged. The exception is keyed on persisted batch membership and leaf role, never merely on `kind = pr_review`.

The current review-pool fixed-Claude policy becomes review-member-aware. Automation keeps its existing Claude/strong policy. Review executions without valid member metadata fail before spawn rather than falling back to Claude and accidentally collapsing diversity.

### Submit structured reports through `boss propose`

Add two proposal kinds:

- `review_report`: one leaf observation, auto-accepted after batch/role/target validation;
- `review_verdict`: the supervisor's consolidated outcome, asynchronously applied because its application may probe PR state and create a remediation work item.

The leaf writes JSON to the engine-owned structured-output path only as a shell-safe `--body-file`, then calls `boss propose review-report --body-file "$BOSS_STRUCTURED_OUTPUT"`. The CLI returns validation errors while the session is still alive, so the reviewer can correct and retry. Completion no longer discovers the report by scraping a transcript.

Attribution is engine-owned. The payload names the batch and target SHA, but the engine derives execution, member role, driver, model, and effort from the socket peer and rejects a mismatch. The default idempotency key is `(batch_id, member_role, attempt)`; a replay returns the existing proposal.

A `review_report` contains:

- batch id, PR URL, target SHA, phase, and a one-paragraph summary;
- coverage: files inspected, files omitted, and limitations;
- findings with severity, category, confidence, file, location, title, problem, impact, suggested fix, and concrete static evidence;
- `needs_runtime_verification`, which must be true when the claim depends on executing code.

The engine keeps the existing severity and category vocabulary so downstream revision rendering and gates remain compatible. A leaf no longer sends an authoritative `revision_warranted` bit; it reports evidence, and the consolidated verdict plus engine gate make that decision.

The accepted report proposal is marked `applied` with its batch member as `applied_ref`. A `review_verdict` remains `proposed` until the verdict reconciler atomically records the durable batch verdict and its clean/remediation result, then marks it `applied` with the verdict or work-item id. This uses the proposal state model's intended asynchronous path rather than blocking the submission socket on GitHub and task creation.

During rollout, the old single-reviewer path remains available behind the batch feature flag. A batch is wholly old-mode or new-mode: leaf reports in a new batch never run the old per-execution finalizer and therefore cannot create three competing revisions. After genuine end-to-end validation and a telemetry soak, remove the transcript parser and direct artifact-to-revision materialization; the structured file remains only an input file to `boss propose`.

### Use a non-verifying supervisor with a two-report quorum

The supervisor is a fourth execution on Claude Sonnet at provider effort `medium`. It starts only after all three leaf roles have either supplied valid reports or exhausted one role-scoped retry.

The supervisor receives the validated report JSON and engine-stamped provenance in its prompt. It does not receive the PR diff, does not get a source checkout as an information source, and runs under a deny-by-default supervisor posture whose only write is `boss propose review-verdict`. This makes “just collating” a real boundary rather than a suggestion.

Its output is bounded: a short summary, source report ids, missing roles, coverage union, and consolidated findings. The target is no more than 4,000 output tokens. Raw reports remain durable, so the supervisor never has to repeat all evidence merely to preserve it.

Collation rules are:

- **Same finding from multiple reviewers:** merge it once by affected location and underlying cause; preserve every source report id and each source severity/confidence. The displayed and gated severity is the maximum reported severity.
- **Finding from one reviewer:** retain it when it has a concrete location/evidence and at least medium confidence. Consensus is supporting provenance, not an admission threshold. Low-confidence unique observations remain advisory.
- **Contradiction:** do not adjudicate from memory and do not inspect source. Emit one `disputed` finding containing both claims and sources. If any concrete medium/high-confidence side carries a severity/category that would pass the existing gate, the disputed finding remains gating; otherwise it is advisory for human review.
- **Malformed or vague observation:** identify the rejected report/finding and reason in the verdict; never silently drop it.

The engine still applies its existing independent severity/category gate to the consolidated findings. The supervisor's `revision_warranted = false` cannot suppress a critical/high finding or a category that already forces remediation.

One failed, timed-out, or empty leaf gets one retry on the same driver/model/profile. After retry exhaustion, two valid reports are sufficient to run the supervisor, with the missing role recorded in the verdict and an observable degraded-batch counter. Fewer than two valid reports cannot produce a clean outcome: pre-merge work remains held with an attention and can be retried, while post-merge work records a failed safety-net batch without changing the already-merged task.

This policy favors availability without turning one provider's opinion into an apparently multi-agent clean bill. It also avoids substituting a different provider for a failed role, which would make the promised driver composition false.

### Apply one verdict and reuse the remediation lifecycle

The `review_verdict` applier replaces per-leaf completion finalization. It validates that every source report belongs to the batch and target SHA, writes one extended `pr_review_verdicts` row for the batch, increments the review cycle once, and chooses one of three outcomes:

- clean/advisory only: mark the batch complete and advance the pre-merge work item to human Review;
- qualifying findings while the origin PR is open: call the existing revision creation path with consolidated instructions and autostart the revision on the existing PR branch;
- qualifying findings after the origin PR merged: materialize the same logical review remediation as an autostart `followup` with origin task/PR provenance, targeting a new PR against `main`.

Extract the parent-close conversion's review-follow-up construction into a shared helper and use it from both the existing conversion path and the verdict applier. This preserves the established property—merged review findings become follow-up work—without manufacturing a temporary `revision` row that violates the invariant “revision implies an open parent PR.”

The proposal id is the materialization idempotency key. Reapplying a verdict returns the same revision/follow-up even if the PR changed state between attempts. If the PR merges during application, the transaction retries through the merged branch and creates the follow-up instead of recording `revision_creation_failed` and discarding findings.

Revision-triggered re-reviews create a fresh pre-merge batch for the new head SHA. The existing maximum review-cycle policy applies to completed batches, not leaf attempts; retries and the supervisor consume no extra cycle.

### Run one deep post-merge integration review

The merge poller's first idempotent transition to merged checks the persisted pre-merge profile. For a Deep batch containing production code, it enqueues one `post_merge_reviewer` member keyed by the origin PR and merge SHA. Existing or legacy PRs without a usable profile are conservatively classified at merge from the same GitHub metadata before this decision.

The post-merge worker uses Claude Opus at provider effort `high`. It is the only reviewer of the landed tree and runs alone rather than as one of three parallel leaves, so it carries no fan-out budget and is deliberately given more effort than a pre-merge leaf. It checks out the actual landed `main` commit, scopes attention to the origin PR's changed paths, and reviews integration with the final surrounding code: merge-resolution loss, callers outside the PR diff, interactions with changes that landed ahead of it, and behavior visible only in the merged tree. It remains static-analysis-only and submits a `review_verdict` directly; a supervisor would add no independent evidence to a single report.

This differs from pre-merge review in target, purpose, and topology. It does not delay or reopen the merged PR. A qualifying verdict creates the follow-up described above, whose ordinary implementation worker builds/tests the fix and opens a new PR against `main`.

Post-merge enqueue is best-effort but durable: one failed execution gets one retry, then an attention and failed batch state. The merge poller never waits for the agent call and never enqueues the same `(origin PR, merge SHA)` twice.

### Expand the static review pool to 16 slots

Raise the review pool's default and maximum from eight to 16, extending its global slot range from 25–32 to 25–40. Update the protocol worker-name mapping and the macOS roster in the same PR so slot identity remains bijective and visible; do not alter slots 1–16 or 17–24.

Admission is weighted inside the review pool:

- a pre-merge batch reserves four units from creation through supervisor completion;
- a post-merge single-reviewer batch reserves one unit;
- retries reuse their batch's reservation;
- pre-merge work has priority over post-merge safety-net work.

With capacity 16, four pre-merge PRs can progress concurrently. Reservation is conservative—the supervisor follows its leaves—but prevents a fifth wave of leaves from occupying every slot just as earlier batches become ready to collate. This is static capacity and static admission accounting, not dynamic pool sizing.

### Roll out as validation of the chosen design

The rollout study validates this architecture's cost, latency, and reliability claims; it is not a study choosing between fan-out and a single reviewer after implementation has already committed to fan-out.

Before default-on rollout, exercise the genuine end-to-end path on a controlled PR with the real Claude, Codex, and Grok drivers, the real proposal socket, scheduler, supervisor, and remediation applier. Unit tests and a hand-built transcript fixture are necessary but cannot stand in for that integration path.

Record per batch:

- queue and awake duration by role and end-to-end;
- input/output tokens and resolved model by member;
- valid report, retry, missing-role, and quorum rates by driver;
- raw versus consolidated finding counts, duplicate groups, and disputes;
- supervisor output tokens and duration;
- clean, revision, merged-race follow-up, and post-merge follow-up outcomes;
- reviewer build-command guard denials;
- pool reservation and queue depth.

The wake-assertion prerequisite must be present before latency is assessed. Default-on is blocked if the genuine path cannot correlate all proposals to one immutable target, if a sub-quorum batch can advance clean, or if findings can be lost at the open-to-merged transition. The initial economic check is supervisor median output below 9,702 tokens and batch awake wall below 7.6 minutes; the design target is materially better at no more than 4,000 supervisor output tokens and roughly 3.7 minutes end to end.

## Alternatives considered

### One execution that fans out to three subagents

Rejected because an execution currently owns exactly one driver process, lease, transcript, permission surface, and proposal identity. A parent process launching other provider CLIs would bypass per-driver spawn policy and make one crash or compromised prompt affect all three reports; native subagents would not cross Claude/Codex/Grok providers. Separate executions reuse the established unit of isolation and make role-scoped retry and attribution checkable in the database.

### Keep one reviewer and choose a stronger model for large PRs

Rejected as the target architecture because it does not provide provider diversity and the supplied baseline is unfavorable: one xhigh reviewer produced a median 34,380 output tokens over 7.6 awake minutes, versus the proposed planning envelope of about 28,678 tokens over 3.7 minutes. Stronger models remain useful inside the Deep profile, but as one member of the trio rather than a substitute for independent reports.

### Deterministically union the three JSON reports

Rejected because exact fingerprints cannot recognize differently worded reports of the same underlying bug, and a union cannot render contradictions as one intelligible outcome. Majority voting is worse: it would suppress a high-quality unique finding, even though diversity is the reason to pay for three providers. The supervisor performs semantic grouping while the engine preserves mechanical severity gates and raw evidence.

### Let the supervisor inspect source and verify disputed findings

Rejected for this version because it turns collation into a fourth review, adds another diff/context load and tool surface, and weakens the cost/latency case. Current practice already relies on the implementation revision and CI to verify actionable findings. Disputes therefore remain visible and conservatively gating when their strongest concrete claim would gate; a future verifier would be a different architecture requiring its own evidence.

### Run the full trio again post-merge

Rejected because the post-merge pass answers a narrower question—whether the landed tree introduces integration defects after an already-diverse pre-merge review. One strong static reviewer against the merge commit is the requested safety net. Repeating all three plus a supervisor would nearly double review spend without a stated requirement or baseline showing that extra diversity after merge is worth it.

## Risks / open questions

- **Threshold calibration:** the initial profile cutoffs are policy, not empirical quality boundaries. Persisting raw inputs allows later calibration without changing historical classifications. Threshold changes must be reviewed as policy changes and must not reuse work-item effort.
- **Grok has no lower current tier:** Light and Deep currently use the same Grok model. This is intentional and visible, but it means variability comes from Claude and Codex until xAI exposes a supported fast model.
- **Supervisor compression could erase nuance:** raw reports remain durable, every consolidated finding cites source report ids, and rejected/malformed observations are enumerated. The 4,000-token target constrains repetition, not evidence retention.
- **Two-report quorum is degraded coverage:** a degraded verdict is visibly labeled and measured. It may create a revision, but can never be represented as three-provider clean coverage.
- **Static pool expansion increases simultaneous provider load:** batch reservations cap pre-merge concurrency at four PRs and post-merge work is lower priority. Dynamic capacity remains deliberately outside this project.
- **Provider/model menus drift:** selection resolves through each driver's model menu and stores the actual resolved model on the member. An unavailable mapping fails that member visibly rather than silently changing providers.
- **Merged-race correctness spans two existing paths:** the shared remediation helper and proposal-id idempotency are load-bearing. Tests must cover open→merged races before and after verdict persistence, plus replay after a revision has already converted to a follow-up.
- **Sleep masks the claimed speedup:** no latency conclusion is valid until the separate wake assertion is deployed. That external dependency is a rollout prerequisite, not an implementation entry here.

No load-bearing product decision remains open for implementation. Model tiers, thresholds, quorum, supervisor authority, post-merge topology, and static capacity are chosen above; telemetry may justify a later policy revision, but it is not permission for the initial implementation to improvise them.

## Proposed implementation task breakdown

Breakdown size: 9 entries (9 in-scope, 0 deferred) — the change has nine reviewable seams across review policy/data, proposal ingress, driver restrictions, orchestration, consolidation, remediation, merge triggering, pool/UI capacity, and genuine-path cutover.

### Add review profiles, model tiers, and batch persistence

Scope: in-scope

Add the pure PR-metadata classifier and its Light/Standard/Deep tests to the review crate; extend each driver model menu with the concrete review-tier mapping; add persisted review batch/member protocol types, schema migration, uniqueness rules, and query APIs. This PR introduces no production fan-out and does not change automation policy.

Effort hint: large

Dependencies: none

Parallelism: may start immediately. It establishes the contract consumed by every later review task.

### Add review report and verdict proposal ingress

Scope: in-scope

Extend the closed proposal vocabulary, typed payload validation, rate caps, idempotency derivation, `boss propose` CLI, and worker-tier authorization for `review-report` and `review-verdict`. Auto-accept reports into their batch member; leave verdict application asynchronous and unimplemented in this PR.

Effort hint: medium

Dependencies: Add review profiles, model tiers, and batch persistence

Parallelism: must follow the batch contract because validation keys on its identities. It precedes prompt migration so no worker is instructed to call a missing verb.

### Harden reviewer capabilities and submit reports in-run

Scope: in-scope

Add the cross-driver reviewer build/test/execution guard, preserve the current mutation/publish fences, update the leaf prompt and structured schema, and make a reviewer write its body file then call `boss propose review-report`. Claims needing execution are marked for runtime verification; missing proposals become explicit member failure rather than transcript recovery.

Effort hint: large

Dependencies: Add review report and verdict proposal ingress

Parallelism: begins after proposal ingress. This task owns driver permission files and reviewer prompt/rendering; later orchestration work must consume rather than duplicate those policies.

### Dispatch and recover three role-aware leaf reviewers

Scope: in-scope

Replace single-execution enqueue/dedup with batch creation, three atomic member executions, member-selected driver/model/effort at spawn, the same-batch read-only chain-guard exception, and role-scoped one-retry recovery. Keep the new mode feature-flagged and ensure old-mode and batch-mode finalizers cannot both act on one target.

Effort hint: large

Dependencies: Add review profiles, model tiers, and batch persistence; Harden reviewer capabilities and submit reports in-run

Parallelism: follows capability hardening because a three-way dispatch must not briefly ship with unrestricted reviewers. It substantially edits coordinator/runner dispatch surfaces, so pool expansion is ordered after it and must forward-port these changes preservingly.

### Add supervisory consolidation and quorum progression

Scope: in-scope

Add the supervisor execution/worker posture, prompt and bounded verdict schema, two-of-three quorum state machine, semantic dedup/source attribution, contradiction handling, and `boss propose review-verdict` submission. Advance a batch only after all roles report or exhaust retry; fewer than two reports must hold rather than produce a clean verdict.

Effort hint: large

Dependencies: Dispatch and recover three role-aware leaf reviewers

Parallelism: follows fan-out because it consumes real member lifecycle. It also touches execution-kind and runner/coordinator matches, so review-pool expansion is explicitly sequenced after this task and must integrate these additions rather than overwrite them.

### Apply consolidated verdicts through unified remediation

Scope: in-scope

Implement asynchronous `review-verdict` application, one verdict/cycle update per batch, clean advancement, and proposal-idempotent remediation. Extract and reuse the review-findings follow-up constructor so an open origin creates a revision and a merged origin creates a follow-up against `main`, including the merge-during-apply race; stop leaf completion from directly creating revisions in batch mode.

Effort hint: large

Dependencies: Add supervisory consolidation and quorum progression

Parallelism: after the supervisor task, this can run in parallel with static pool expansion because it primarily edits proposal/completion/revision helpers rather than coordinator slot and app-roster files.

### Trigger the landed-code post-merge review

Scope: in-scope

Extend the merge probe/transition with idempotent Deep-production eligibility and merge-SHA batch creation; dispatch one Opus/high post-merge reviewer against the real landed tree, retry once, and route its verdict through unified follow-up materialization without blocking the merge poller.

Effort hint: large

Dependencies: Apply consolidated verdicts through unified remediation

Parallelism: may run in parallel with static pool expansion after unified remediation exists. It owns merge-poller integration and must reuse, never reimplement, batch classification and follow-up construction.

### Expand the static review pool and add weighted admission

Scope: in-scope

Raise the review pool default/maximum to 16, extend slot identities through 40 in protocol and macOS roster code, and add four-unit pre-merge versus one-unit post-merge reservation accounting with pre-merge priority. Interactive and automation ranges and admission behavior remain byte-for-byte equivalent in behavior.

Effort hint: medium

Dependencies: Add supervisory consolidation and quorum progression

Parallelism: may run in parallel with verdict application and post-merge work once the supervisor task lands. It is ordered after the fan-out/supervisor coordinator edits because those tasks substantially overlap the same dispatch files; forward-port them preservingly.

### Validate the genuine path, cut over, and remove legacy finalization

Scope: in-scope

Add batch-level observability and operator diagnostics, exercise a controlled PR through the real three drivers, proposal socket, supervisor, revision path, merged-race follow-up, and post-merge path, then enable batch mode and remove transcript scraping plus direct artifact-to-revision finalization after fallback telemetry is quiet. The external wake assertion must be deployed before latency acceptance; tests must prove build-command denials and sub-quorum fail-closed behavior.

Effort hint: large

Dependencies: Trigger the landed-code post-merge review; Expand the static review pool and add weighted admission

Parallelism: final integration and cutover task; it cannot run in parallel with unfinished implementation entries because only the genuine assembled path can validate the design's claims.
