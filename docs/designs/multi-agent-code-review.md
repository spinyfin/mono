# Multi-agent code review: independent reports, a source-reading supervisor

- Date: 2026-10-05 (as-built review; original design 2026-09-01)
- Status: implemented behind a default-off flag; acceptance, cutover, and legacy removal incomplete
- Provenance: project design for Multi-agent code review
- Related designs: [automated reviewer pass](../../tools/boss/docs/designs/automated-reviewer-pass-on-every-agent-authored-pr.md), [worker proposal API](../../tools/boss/docs/designs/worker-proposal-api-replace-fragile-worker-to-engine-seams.md), [revision tasks](../../tools/boss/docs/designs/revision-tasks.md), [unified PR remediation](../../tools/boss/docs/designs/unify-pr-remediation-on-revisions.md)

The contested property is supervisor authority: the shipped supervisor can inspect source and adjudicate disagreements, contrary to the original non-verifying collation boundary. Independent leaf reports, per-role recovery, and one durable verdict shipped; the original evidence-preservation and cost claims do not automatically carry over to that broader role.

## Verdict

When enabled, batch review uses parallel Claude and Codex leaves followed by a Claude supervisor. Frozen PR profiles select models, reports and verdicts travel through `boss propose`, and qualifying consolidated findings create an open-PR revision or a closed-origin follow-up. A solo Opus reviewer checks eligible landed commits.

The static pool has 16 review slots and four-unit pre-merge reservations. The code default remains off, the legacy finalizer remains reachable, and post-merge eligibility, admission recovery, and reservation accounting have the limitations below. Pipeline availability does not establish that rollout acceptance passed.

## Evidence and scope

This review compared PR bodies, commit history, and diffs with repository state at `74dedeaa`:

| Delivery                                          | Merged evidence                                                                                          |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Profiles, model tiers, persistence                | [#2851](https://github.com/spinyfin/mono/pull/2851)                                                      |
| Proposal ingress and reviewer restrictions        | [#2854](https://github.com/spinyfin/mono/pull/2854), [#2855](https://github.com/spinyfin/mono/pull/2855) |
| Fan-out, quorum, supervisor                       | [#2861](https://github.com/spinyfin/mono/pull/2861), [#2881](https://github.com/spinyfin/mono/pull/2881) |
| Verdict application and remediation               | [#2897](https://github.com/spinyfin/mono/pull/2897)                                                      |
| Landed-code trigger and pool expansion            | [#2909](https://github.com/spinyfin/mono/pull/2909), [#2896](https://github.com/spinyfin/mono/pull/2896) |
| Diagnostics and component acceptance tests        | [#2917](https://github.com/spinyfin/mono/pull/2917)                                                      |
| Supervisor recovery and post-merge cleanup repair | [#2895](https://github.com/spinyfin/mono/pull/2895), [#2938](https://github.com/spinyfin/mono/pull/2938) |

Code inspection establishes implementation, not a repeated production acceptance run. The original three-leaf delivery was subsequently extended with explicit generations ([#2979](https://github.com/spinyfin/mono/pull/2979)) and changed to two leaves ([#3073](https://github.com/spinyfin/mono/pull/3073)); those contracts already present in this document remain below. No production database or live flag override was inspected, so default-off does not mean no deployment enabled batches.

## Two-leaf decision (2026-10-03)

Across 389 pre-merge batches from 2026-09-09 to 2026-10-03, Claude/Codex/Grok produced 2.8/1.7/1.2 findings per batch, no findings in 20%/28%/43% of batches, 693/260/140 unique findings, unique critical/high findings in 8%/15%/7% of batches, contradiction wins of 50%/38%/22%, and median runtimes of 3.3/2.9/8.4 min. Grok finished last in 97% of rounds, adding about 5 minutes to every review without increasing its unique high-severity yield with runtime.

The supervisor kept 97%/83%/90% of Claude/Codex/Grok findings; Grok alone supplied 4 of the 10 retained critical findings (including mono#2927). This trades those few unique serious catches for removing the delay, with Codex catching more unique critical/high issues, faster.

These figures come from every report and verdict in an engine-database snapshot taken 2026-10-03 00:21 CDT; automatic finding matches were about 95% correct on a spot check. The always-Claude supervisor uses the Claude reviewer's model, potentially flattering Claude, and there is no record of which findings were real or fixed. Grok remains available as an implementation-worker driver.

## Goals

- Run two independent pre-merge reviews in parallel: one each on the Claude and Codex drivers.
- Keep leaf reviews cheap and bounded at provider effort `medium`, while varying the model from the PR's own size and complexity rather than the parent work item's effort.
- Produce one consolidated, attributable verdict without majority-vote suppression of unique findings.
- Continue the existing review/revise loop, counting one completed batch—not two leaf executions—as one review cycle.
- Trigger a deeper review of the landed code for large or complex production PRs and turn qualifying findings into an automatically dispatched follow-up against `main`.
- Enforce a static-analysis-only reviewer posture: no edits, pushes, GitHub writes, builds, tests, formatters, generators, or execution of changed code.
- Move reviewer findings delivery from completion-time artifact/transcript parsing to the typed, validated, idempotent `boss propose` channel.
- Preserve the current user-facing AI-review state and revision/follow-up provenance while adding enough batch detail to diagnose provider failures and model-selection decisions.

## Non-goals

- Dynamic pool sizing or changes to interactive and automation pool behavior.
- Fixing host sleep. The separate wake-assertion work is a prerequisite for realizing the latency benefit, not part of this project.
- Changing the work-item `--effort` classifier or using work-item effort as a review-size proxy.
- Posting leaf reports, supervisor discussion, or findings to GitHub.
- An unrestricted third source review. The shipped supervisor may inspect source to resolve leaf disagreements, broader than the original non-verifying boundary.
- Giving review agents permission to build or run tests in exceptional cases.
- Replacing human PR review or making a clean automated verdict sufficient to merge.
- Running the two-leaf pre-merge topology again after merge. Post-merge review is a distinct, single-agent integration safety net.

## Constraints and baseline

The legacy path routes review to Claude/strong and finalizes a `ReviewResult` artifact or transcript through [`finalize_pr_review_pass`](../../tools/boss/engine/core/src/completion/finalize_passes.rs). It remains reachable when batch mode is disabled; batch-member completion bypasses it. Automation keeps its separate pool policy.

The batch path reuses peer-derived proposal attribution, canonical validation, the durable proposal ledger, and the existing remediation lifecycle. An open-origin finding becomes a revision; a merged or closed-unmerged origin becomes a follow-up with origin provenance, matching established parent-close conversion.

Read-only checkout access and permission to execute code are separate properties. #2855 added a shared reviewer static-analysis command guard across Claude, Codex, and Grok, including legacy reviewers. Codex uses an output-only `workspace-write` sandbox configuration, with the checkout outside writable roots, so it can write the proposal body. Reviewer prompts and guards prohibit builds, tests, formatters, generators, changed-code execution, repository edits, pushes, and GitHub writes. Findings needing execution carry `needs_runtime_verification`; implementation workers perform that verification.

### Measured performance baseline

The supplied baseline covers 434 executions, including 52 deeply parsed transcripts. It shows near-constant throughput of about 3,600 output tokens per minute and the following effort medians:

| Provider effort | Awake wall time | Turns | Output tokens | Context peak |
| --------------- | --------------: | ----: | ------------: | -----------: |
| low             |         1.1 min |     6 |         4,531 |          58k |
| medium          |         2.6 min |    12 |         8,226 |          72k |
| high            |         5.9 min |    21 |        26,382 |         107k |
| xhigh           |         7.6 min |    25 |        34,380 |         137k |

The original three-leaf planning envelope budgeted 24,678 output tokens and about 2.6 minutes of parallel awake time. Its non-verifying supervisor target was no more than 4,000 output tokens, adding about 1.1 minutes at the measured throughput: approximately 28,678 output tokens and 3.7 minutes end to end, versus 34,380 tokens and 7.6 minutes for one xhigh reviewer. Source-reading supervision changes that premise; these projections are retained as historical hypotheses.

The output-token advantage holds while the supervisor stays below 9,702 tokens; the awake-wall advantage holds while it stays below about five minutes. The 4,000-token target leaves meaningful margin on both. This is a planning envelope, not a measured supervisor result, and it excludes provider price differences and input-token cost; rollout telemetry must validate it.

Host sleep in the supplied baseline consumed 38% of measured review wall time, and 95% of gaps longer than 120 seconds were attributable to sleep. Concurrent reviewers suspend together, so concurrency does not recover that lost wall time. Wake-assertion deployment must be verified before assessing latency; this repository review does not establish deployment status.

## Shipped architecture

### Persist immutable review batches with explicit generations

The persisted `pr_review_batches` and `pr_review_batch_members` tables have shared protocol types, migrations, and transactional query/create APIs. Roles are explicit membership data.

A batch records:

- identity: batch id, cycle-root work item, PR URL and number, generation (starting at one);
- phase: `pre_merge` or `post_merge`;
- immutable target: base SHA, reviewed head SHA, and merge SHA when applicable;
- the complete classification input and selected profile;
- lifecycle: collecting, supervising, applying, completed, or failed;
- timestamps and the final verdict/proposal id.

Batch members record batch id, role, execution id, requested driver, resolved model, provider effort, attempt number, and terminal/report state. Pre-merge roles are `claude_reviewer`, `codex_reviewer`, and `supervisor`; post-merge uses `post_merge_reviewer`. The unique key `(batch_id, role, attempt)` makes recovery retries explicit, while `(cycle_root_id, phase, target_sha, generation)` identifies each separate batch. Existing rows migrate to generation one. Post-merge batches stay at generation one and retain their existing deduplication behavior.

Immutability is a property of a batch, not of a target SHA. With `review_batch_fanout` enabled, `bossctl review start --pr <n>` creates both heterogeneous leaf members atomically using the automatic path's classification and admission logic; the existing quorum lifecycle dispatches the supervisor after the leaves settle. An explicit request at a head whose latest batch is completed or failed creates the next generation, leaving every prior batch, member, and execution untouched. A request while that head's batch is active reuses it. Explicit re-review never grows an admitted batch or adds reviewer attempts to it.

Only explicit `bossctl review start` invocations advance generations. Automatic post-push admission reuses the latest existing batch at a head, including a completed batch, and never creates an automatic re-review generation. Explicit starts override pure-rebase, no-op, already-reviewed-head, and maximum-cycle redundancy skips, but still require the four-unit reservation. Capacity exhaustion, unavailable PR metadata, or an unavailable reviewer driver returns an error without a single-reviewer fallback. An active legacy reviewer also returns an error, keeping the two finalizers from competing. With the flag off, `bossctl review start` retains its legacy single-reviewer behavior; the feature flag is the only mode switch.

Each batch also persists whether it was minted by an explicit `bossctl review start` admission. Verdict application's same-head duplicate-suppression guard is scoped to automatic and legacy batches only: an explicit admission is a deliberate request to re-review one exact head, so a prior pass at the same head does not suppress its qualifying verdict; the other remediation holds still apply.

Resolved model and effort belong to the member, not the task. The scheduler reads the member policy at spawn, so review configuration cannot inherit `tasks.driver`, `tasks.model_override`, `tasks.reasoning`, or `tasks.effort_level`. The batch preserves both classifier inputs and the resolved model names, making a later menu or threshold change unable to rewrite history.

### Compute size and complexity once, before dispatch

The engine computes a `ReviewProfile` when it first creates the batch, using the PR's own GitHub file metadata. It performs this before member executions exist, persists the result, and reuses that same snapshot for both leaves, the supervisor, re-review-cycle accounting, and post-merge eligibility.

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

The intended post-merge boundary is Deep **and** production code. Classification persists both facts, but the shipped trigger checks only Deep; the missing consumer is recorded below.

### Map profiles through each driver's real model menu

Driver model menus expose review-specific `fast`, `balanced`, and `strong` mappings. Current mappings are:

| Review profile | Claude   | Codex         | Provider effort |
| -------------- | -------- | ------------- | --------------- |
| Light          | `sonnet` | `gpt-6-astra` | `medium`        |
| Standard       | `sonnet` | `gpt-6-astra` | `medium`        |
| Deep           | `opus`   | `gpt-6-astra` | `medium`        |

The Claude mapping follows the requested example: small/simple work uses Sonnet, while large/complex work earns Opus. Codex selects `gpt-6-astra` for Light, Standard, and Deep.

Every pre-merge leaf and supervisor receives provider effort `medium`, independent of the parent task. The supervisor uses the same Claude profile mapping: Sonnet for Light/Standard and Opus for Deep, instead of fixed Sonnet. #2881 records a preference for a consistent Claude judge, but no economic justification for dropping the fixed cheap model. Post-merge stores effort `large`, which resolves to Claude provider effort `high`; the names are equivalent only on that resolved-effort dimension.

### Dispatch two executions, not one execution with subagents

The batch reconciler atomically inserts one member and one `pr_review` execution per driver, then kicks the scheduler. Each execution gets its own provider process, lease, transcript, proposal attribution, retry state, and review-pool slot.

The per-PR chain guard permits compatible read-only members of the same pre-merge batch: leaf/leaf and leaf/supervisor pairs, never supervisor/supervisor pairs. This lets consolidation proceed while a reporting leaf finishes teardown. They still block every writer and reviewers from other batches. Explicit generations at the same target follow terminal batches, while the existing conflict-resolution preemption remains unchanged. The exception is keyed on persisted batch membership and compatible roles, never merely on `kind = pr_review`.

Spawn policy reads persisted member policy and fails invalid batch metadata rather than collapsing diversity to Claude. Genuine legacy executions retain their path; the current spawn guard prevents a memberless legacy reviewer from competing with a live pre-merge batch. Automation retains its Claude/strong policy.

### Submit structured reports through `boss propose`

Two proposal kinds ship:

- `review_report`: one leaf observation, auto-accepted after batch/role/target validation;
- `review_verdict`: the supervisor's consolidated outcome, asynchronously applied because its application may probe PR state and create a remediation work item.

The leaf writes JSON to the engine-owned structured-output path only as a shell-safe `--body-file`, then calls `boss propose review-report --batch-id <batch> --target-sha <sha> --body-file "$BOSS_STRUCTURED_OUTPUT"`. The CLI returns validation errors while the session is still alive, so the reviewer can correct and retry. Completion no longer discovers the report by scraping a transcript.

Attribution is engine-owned. The payload names the batch and target SHA, but the engine derives execution, member role, driver, model, and effort from the socket peer and rejects a mismatch. The default key hashes execution id, proposal kind, and canonical payload; identical submissions replay. Membership ownership and the accepted-report link separately enforce one accepted report per attempt. This differs from the proposal-id key used to materialize remediation.

A `review_report` contains:

- batch id, PR URL, target SHA, phase, and a one-paragraph summary;
- coverage: files inspected, files omitted, and limitations;
- findings with severity, category, confidence, file, location, title, problem, impact, suggested fix, and concrete static evidence;
- `needs_runtime_verification`, which must be true when the claim depends on executing code.

The engine keeps the existing severity and category vocabulary so downstream revision rendering and gates remain compatible. A leaf no longer sends an authoritative `revision_warranted` bit; it reports evidence, and the consolidated verdict plus engine gate make that decision.

The accepted report proposal is marked `applied` with its batch member as `applied_ref`; retention preserves its report-owning proposal linkage. Supervisors and solo post-merge reviewers use `boss propose review-verdict --batch-id <batch> --verdict-file <path>`. A `review_verdict` remains `proposed` until the verdict reconciler atomically records the durable batch verdict and its clean/remediation result, then marks it `applied` with the verdict or work-item id. This uses the proposal state model's intended asynchronous path rather than blocking the submission socket on GitHub and task creation.

During rollout, the legacy path remains available when the batch flag is off. A target uses legacy or batch finalization: leaf reports in a new batch never run the old per-execution finalizer and therefore cannot create two competing revisions. After genuine end-to-end validation and a telemetry soak, remove the transcript parser and direct artifact-to-revision materialization; the structured file remains only an input file to `boss propose`.

### Consolidate with source access and a two-report quorum

#2881 shipped a Claude supervisor as an ordinary `pr_review` execution with a `supervisor` member role. It receives accepted leaf findings, summaries, and coverage counts/limitations in its prompt, plus a checkout and explicit permission to re-read source to settle contradictions. It shares leaf static-analysis and mutation restrictions; read-only is not equivalent to non-verifying on the dimension of information access.

The shipped [`SupervisorVerdict`](../../tools/boss/engine/pr-review/src/supervisor_types.rs) contains batch/PR/target/phase identity, summary, `revision_warranted`, consolidated findings, and contradictions. Each finding has severity, category, confidence, file/location, title/detail, and nonempty `sources` naming reviewer **roles**, not report/finding ids. Contradictions store role/claim positions, a resolution, and optional `resolved_in_favor_of`. Validation checks the schema, nonempty sources, at least two contradiction positions, and valid winners; application checks citations against accepted reports in that batch.

The prompt requests semantic deduplication, independent judgment rather than voting, and explicit contradiction resolution from source where possible. Missing legacy roles are named, never treated as clean. Raw reports remain durable, but the verdict has no coverage union, per-source severity/confidence, rejected-finding ledger, or structured `disputed` finding. Schema/size validation does not enforce semantic deduplication, complete retention, maximum source severity, or the original 4,000-output-token target.

The engine projects **consolidated findings** into `ReviewResult` for the existing severity/category gate. A false `revision_warranted` cannot suppress a qualifying finding present there. This does not preserve every qualifying raw claim: contradictions are not independently gated, and application does not recover omitted or downgraded leaf findings. The original retention and conservatively gating dispute promises remain unresolved, not implemented guarantees. The reviewed history explains source adjudication but records no decision assessing that change against the original cost and evidence-preservation requirements.

All configured leaves must settle before consolidation. New batches require both Claude and Codex reports; persisted three-leaf batches retain Grok and two-of-three quorum. A missing/failed report gets one retry on the same driver/model/effort without rerunning successful siblings. An empty **findings list in a valid report** is clean evidence; absence of a report is failure. Insufficient quorum fails with `pr_review_quorum_failed` attention and cannot produce a clean verdict.

#2895 extended recovery to supervisors orphaned without Stop and changed the initial no-retry policy to one retry. Attempt two is terminal. Recovery uses accepted reports and the frozen target; a known moved head or closed/merged PR settles the dead pre-merge supervisor's batch with attention. An unknown live head is not evidence of movement. This recovery requires durable execution death state; it does not itself detect phantom still-running workers.

### Apply one verdict and reuse the remediation lifecycle

The `review_verdict` applier replaces per-leaf completion finalization in batch mode. It validates source roles against accepted reports for the batch and target, writes one extended `pr_review_verdicts` row per batch, and updates cycle accounting at verdict application, never per leaf or retry. The cycle increment is suppressed when `last_reviewed_sha` already matches, including an explicit new generation at the same head. It chooses one of three outcomes:

- clean/advisory only: mark the batch complete and advance the pre-merge work item to human Review;
- qualifying findings while the origin PR is open: call the existing revision creation path with consolidated instructions and autostart the revision on the existing PR branch;
- qualifying findings after the origin PR merged or closed unmerged: materialize the same logical review remediation as an autostart `followup` with origin task/PR provenance, targeting a new PR against `main`.

#2897 extracted the parent-close conversion's review-follow-up constructor into a shared helper used by both paths. This preserves the established property—merged review findings become follow-up work—without manufacturing a temporary `revision` row that violates the invariant “revision implies an open parent PR.”

The proposal id is the materialization idempotency key. Reapplying a verdict returns the same revision/follow-up even if the PR changed state between attempts. If the PR merges during application, the transaction retries through the merged branch and creates the follow-up instead of recording `revision_creation_failed` and discarding findings.

Application preserves merged-parent deletion-signoff holds. A retry discovering that hold tombstones an already-created remediation and cancels its executions; superseded verdict materializations receive the same cleanup. Submission performs no GitHub probe: immediate asynchronous apply and a periodic sweep recover proposed verdicts.

Revision-triggered re-reviews create a fresh pre-merge batch for the new head SHA. The existing maximum review-cycle policy applies to completed batches, not leaf attempts; retries and the supervisor consume no extra cycle.

### Run one deep post-merge integration review

#2909 captures the actual merge commit and creates one `post_merge_reviewer`. Cube's revision-target positioning checks out the landed commit, including squash merges after head-branch deletion. The immutable target satisfies `target_sha == merge_sha`; the old PR head is not a safe substitute because it may no longer be fetchable. Missing merge SHA logs a warning and skips creation; missing base SHA is logged and stored as empty.

The worker is Claude Opus with stored effort `large` (resolved `high`). Its static review focuses on changed paths and integration with the landed tree, including merge-resolution loss and surrounding callers. It submits a verdict directly, moving `collecting` to `applying` without a supervisor. Its source attribution identifies its own Claude driver, not independent corroboration. Failed execution gets one retry, then attention and failed batch state without reopening the merged work item. Findings use the shared follow-up applier.

The [eligibility trigger](../../tools/boss/engine/core/src/merge_poller/post_merge_review.rs) loads the cycle root's latest pre-merge classification and checks `profile == Deep`. It does **not** check `has_production_code`, and skips roots without a pre-merge batch instead of classifying legacy metadata at merge. The [test](../../tools/boss/engine/core/src/merge_poller/tests/post_merge_review_trigger_tests.rs) `no_post_merge_review_without_a_pre_merge_batch_on_record` pins that reduced scope. No reviewed product decision accepts these departures from the promised Deep-production safety net.

Creation is idempotent once a batch exists, keyed by cycle root, phase, merge SHA, and generation one. Enqueue itself is not durably retried: the merge sweep marks the root done before triggering, while `AdmissionDeferred` only logs and writes no pending intent. Normal merge candidates are `in_review`, and the deferred-admission sweep covers pre-merge work. Capacity or transient trigger failures can therefore lose a safety-net pass; idempotent insertion is not equivalent to eventual admission.

#2938 reports that no post-merge review had completed before its fix: three cleanup paths killed reviewers because their cycle root was done. It added phase-aware exemptions to batch reaping, live terminal-work cleanup, and startup abandonment. The invariant is that **a live post-merge execution may validly belong to a completed root**; deleted roots, terminal executions, and genuinely stale batches still receive cleanup. Regression tests cover those decisions; the PR explicitly did not perform a live merged-PR completion check.

### Expand the static review pool to 16 slots

#2896 expanded review slots from 25–32 to 25–40 and updated protocol/macOS worker-name rosters together. Interactive slots 1–16 and automation slots 17–24 retain their behavior.

Pre-merge batches reserve four units through `collecting`, `supervising`, and `applying`; retries reuse the reservation. Four remains conservative after the two-leaf change, accommodating historical three-leaf batches. Pre-merge admission uses configured capacity clamped to 4–16. The floor permits one batch even with fewer than four physical slots; the scheduler still enforces actual execution capacity.

Pre-merge admission counts only other pre-merge reservations; post-merge admission counts both phases. This gives admission priority, not preemption of running safety-net workers. Full pre-merge capacity leaves the producer pending review with an idempotent attention marker; a dedicated sweep retries and recognizes already-reviewed heads. Reaping publishes execution-terminal events so releasing reservations also tears down workers.

Two accounting gaps remain in [`review_batches.rs`](../../tools/boss/engine/core/src/work/review_batches.rs). `reserved_batch_count_in_tx` excludes all done/archived roots, so normal active post-merge batches do not count toward their one-unit budget even after #2938 allowed them to survive. `create_post_merge_review_batch` uses default capacity rather than live configured capacity. These cases do not enforce the intended post-merge reservation policy.

### Rollout evidence and diagnostics

The planned study validates this architecture's cost, latency, and reliability; it was not designed to choose fan-out over a single reviewer. The retrospective two-leaf decision is a separate provider-contribution comparison with its stated lack of ground truth. Neither validates the original cheap, non-verifying supervisor envelope for the source-reading implementation.

#2917 delivered `bossctl review batches <work-item>` and `bossctl review live-batches`, both with JSON output. Per-item lookup resolves revisions to their cycle root; live listing shows nonterminal batches oldest first. Diagnostics expose persisted model/driver/effort, attempts, member states, proposal links, and timestamps. Behavioral Codex/Grok build-denial tests and zero-report fail-closed coverage also landed. These verify components, not the genuine end-to-end path.

That PR explicitly deferred the real-driver controlled-PR exercise, default-on cutover, and legacy-finalizer removal. [The flag registry](../../tools/boss/engine/feature-flags/src/lib.rs) still defaults `review_batch_fanout` off, and `finalize_pr_review_pass` retains legacy artifact/transcript handling. A deployment operator can enable the flag; this review neither reads nor changes live overrides. The rollout gate could block default-on deployment, not earlier optional deployments or the implementation phase itself.

Remaining acceptance must use real providers, the actual proposal socket, scheduler, supervisor, and remediation path on controlled PRs. Record immutable-target correlation, sub-quorum holds, open-PR revision, merge-during-apply follow-up, and post-merge verdict survival across cleanup. Verify the wake assertion before comparing latency. Collect per-role/end-to-end queue and awake time, models and input/output tokens, retry/quorum rates, raw/consolidated/disputed findings, guard denials, reservations, and deferred admission. Rows and gauges supply some inputs; the reviewed PRs contain no completed economic acceptance report.

The original targets—supervisor median output below 9,702 tokens and batch awake wall below 7.6 minutes, with goals of 4,000 tokens and roughly 3.7 minutes—are historical hypotheses, not measured shipped results. Source access and profile-selected supervisor models require an explicit decision about whether those bounds and evidence-preservation requirements still govern rollout.

## Alternatives considered

### One execution that fans out to two subagents

Rejected because an execution currently owns exactly one driver process, lease, transcript, permission surface, and proposal identity. A parent process launching other provider CLIs would bypass per-driver spawn policy and make one crash or compromised prompt affect both reports; native subagents would not cross Claude/Codex providers. Separate executions reuse the established unit of isolation and make role-scoped retry and attribution checkable in the database.

### Keep one reviewer and choose a stronger model for large PRs

This remains the fallback when batch mode is disabled. It lacks cross-provider evidence diversity, the specific property sought by fan-out; the runtime baseline does not prove inferior accuracy. The original three-leaf envelope projected 28,678 output tokens and 3.7 awake minutes versus a single xhigh reviewer's measured 34,380 and 7.6. That projection assumed non-verifying collation, not the shipped system or a controlled comparison.

### Deterministically union the two JSON reports

Rejected because exact fingerprints cannot recognize differently worded reports of the same underlying bug, and a union cannot render contradictions as one intelligible outcome. Majority voting is worse: it would suppress a high-quality unique finding, even though diversity is the reason to pay for two providers. The supervisor performs semantic grouping while the engine preserves mechanical severity gates and raw evidence.

### Non-verifying collation versus source adjudication

The original design rejected source inspection because it adds context and tools and weakens the cost/latency case; implementation revisions and CI already verify actionable claims. #2881 nevertheless shipped source adjudication to settle disagreements. Downstream remediation can test a proposed fix, but does not determine which claims the earlier verdict should retain.

This records shipped behavior, not retroactive approval of the tradeoff. The missing decision is whether supervisor authority may weaken mandatory retention and conservative dispute gating, and what evidence/economic bounds replace them. No reviewed record resolves that question; schema tests cannot answer it.

### Run the full review batch again post-merge

Rejected because the post-merge pass answers a narrower question—whether the landed tree introduces integration defects after an already-diverse pre-merge review. One strong static reviewer against the merge commit is the requested safety net. Repeating both leaves plus a supervisor would nearly double review spend without a stated requirement or baseline showing that extra diversity after merge is worth it.

## Risks and remaining work

These gaps are separate from durable reasoning above. The former nine-entry implementation plan is no longer a status list: the pipeline and diagnostics landed, recovery expanded, and final acceptance/cutover scope did not finish.

- **Resolve supervisor authority and evidence preservation.** Decide source access, maximum-severity retention, qualifying unique findings, unresolved disputes, rejection provenance, and fixed-Sonnet/token assumptions. Align prompt, schema, gating, and tests together. Current source validation establishes report attribution, not preservation of every qualifying claim.
- **Complete genuine-path acceptance and gated cutover.** Validate the real pipeline, including survival after #2938, collect economic/outcome evidence, verify the external wake prerequisite, then finish default-on rollout and legacy removal after the intended soak. #2917 explicitly deferred this; the default and finalizer confirm it remains incomplete. An authorized operator or suitably isolated integration environment needs real provider and GitHub capabilities.
- **Finish post-merge eligibility.** Consume production-code presence as well as Deep, and classify legacy/no-profile origins as promised. Replace tests pinning silent exclusion alongside the implementation; those tests do not establish a product decision.
- **Make post-merge reservations accurate.** Count live post-merge batches on done roots and use configured capacity. Preserve deleted-root cleanup, pre-merge priority, and physical slot limits. #2938 fixed reaping but not the reservation query.
- **Persist and retry post-merge admission intent.** Deferred/transiently failed triggers must remain discoverable after the origin becomes done and across restart, without duplicating a merge-SHA batch. Log-only deferral does not deliver durable enqueue.

Thresholds remain policy rather than calibrated quality boundaries; frozen inputs and resolved models make later changes auditable. The two-leaf switch trades some unique Grok findings for latency and requires both remaining providers to report. Its measurement limits remain above rather than becoming a claim of equivalent review quality.
