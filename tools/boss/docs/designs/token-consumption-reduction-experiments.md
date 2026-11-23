# Token consumption reduction experiments: arms are assigned per PR family and recorded in the launch record

- **Date:** 2026-11-23
- **Status:** Design. Nothing in this document is implemented.
- **Product:** Boss. **Project:** Token consumption reduction experiments.
- **Deliverable:** this document. The task breakdown at the end is the handoff to scheduling.
- **Source:** the investigation `tools/boss/docs/investigations/token-consumption-reduction-2026-11-19.md`, on mono#3751 (open on this date). R1–R10, C1–C12, "gap N" and "follow-up N" below are its numbering.
- **Evidence base:** every code claim was checked against `spinyfin/mono` `main` at `e2381ab3` on 2026-11-23. Paths are under `tools/boss/` unless they start at the repository root. Volumes and savings are the investigation's, for its measured week (2026-11-11 to 2026-11-18). Nothing was re-measured and no production state was read.
- **Effort names** are Boss effort levels (`trivial`, `small`, `medium`, `large`), as in the investigation. The Claude and Codex drivers both send `small` as provider effort `medium`, `medium` as `high` and `large` as `xhigh`. Where a provider value is meant, the text says so.
- **Related:** [`token-usage-window.md`](token-usage-window.md) (the measuring tool), [`trustworthy-per-work-item-cost-attribution.md`](trustworthy-per-work-item-cost-attribution.md) (capture defects and the rate table), [`../conflict-resolution-deferral.md`](../conflict-resolution-deferral.md) and [`../driver-traffic-and-api-key-settings.md`](../driver-traffic-and-api-key-settings.md) (the two A/B experiments Boss already runs).

Boss used about 9.7 billion tokens in the investigation's measured week. This design ships each of its recommendations as a change that can be switched on for half of the work, compared with the other half in the Token Usage window, and switched off in one click. It also says which recommendations cannot be run that way, and what has to be recorded before any comparison can be read.

## Verdict

- **One mechanism for all of them.** An experiment is a pair of feature flags plus one registry entry. The engine reads the flags on every launch and every policy decision, assigns an arm, and records it. No experiment gets its own setting, table, request or report.
- **The contested property: the unit is the PR family, not the run.** A work item and all of its revisions, reviews and guides share one arm of each experiment. A reviewer could reasonably prefer splitting by execution id, which gives about six times as many units. The reason not to is that the investigation's own yardstick (tokens per merged PR) and every quality guardrail are per PR, and half of the recommendations work by removing runs, which leaves no execution to assign.
- **Arms live in the launch record.** `work_executions` already records the driver, model and effort a run launched with. The arm set is one more column on that row, written by the same writer in the same transaction.
- **Each experiment is read at the pipeline stage it changes.** A 5% saving cannot be seen in whole-PR tokens within a month at Boss's volume. It can be seen within a week on the runs it actually touches, so the usage rows gain a pipeline-stage dimension next to the arm dimension.
- **Scope.** 19 experiments (one ships with the mechanism, 18 in 12 experiment entries), 2 straight changes, 11 entries of recording and reading that the experiments depend on, and 3 deferred entries for experiments the investigation recommends but that wait on something else. Three steps the investigation words as "consider" or "if" are recorded with their conditions and not filed (section 8).

## Goals

- Ship every recommendation R1–R10 as a change in the repository. Where a control arm can exist, ship it as a flag-controlled experiment that is off until the repo owner turns it on.
- One way to start, split and stop an experiment, built on the flag store Boss already has.
- Every run durably records which arm of each running experiment it launched under, in the launch record the Token Usage window already reads.
- The Token Usage window and `boss cost rows` can compare treatment with control by pipeline stage, driver and model, with every figure computed by the engine.
- Every experiment has a named quality guardrail and a condition for ending it early, both readable from data Boss records.
- The instrumentation the comparisons need lands before the experiments that need it.

## Non-goals

- **Reading the results.** Deciding that an experiment worked is the repo owner's job with the Token Usage window. No entry here has "measure the effect" as its deliverable.
- **Adoption changes.** Making a treatment the default is one small change per adopted experiment, filed when its result is read. None is pre-filed.
- **Statistics in the engine.** The engine reports counts, sums and ratios per arm. It does not compute significance.
- **Configurable split shares and more than two arms.** Every split is half and half. See alternative D.
- **Moving the two existing experiments** (just-in-time conflict resolution, the Codex Sol share) onto the new mechanism. They keep working as they are.
- **Workers on remote hosts.** The remote spawn path reads no feature flags today (Finding 8). Remote runs record an empty arm set and are never treated.
- **Grok-specific reductions.** The investigation makes none, because it could not measure Grok at the time. See Risks.
- **Changing the Deep classifier** that picks the pre-merge reviewer's model. R9 says not to until R1 and R2 are read.
- **Capture defects owned by the cost-attribution design**: excluded Claude subagent tokens, Codex gross input, and the dated rate table.
- **Ideas the investigation names outside R1–R10, its gaps and its follow-up list**: the shorter cache lifetime for review roles, quota-aware dispatch, batching post-merge follow-ups per area, and phase-boundary handoffs for very large runs. Each waits on a measurement this project produces. None gets an entry.
- **Steps inside R1–R10 that the investigation makes conditional**: a review cap of two rounds, keeping engine-minted revisions off the Sol model, and batching the coordinator's inbound messages. Section 8 records the condition for each and the entry whose output decides it. None gets an entry until its condition is observed.

## Background: what exists, verified against the code

### Finding 1: feature flags are the live switch; the review settings are not

`boss-feature-flags` (`engine/feature-flags/`) is a registry of named boolean flags with defaults, a store that mirrors overrides to `feature-flags.toml`, and one consumer call, `is_enabled("name")`, which reads the in-memory value on every call. The macOS app lists every registered flag under Debug → Feature Flags, grouped by category, and sets one with the `SetFeatureFlag` request. Adding a flag needs no protocol or app change. No `boss` or `bossctl` verb sets a flag.

The three review settings the investigation cites (`BOSS_MAX_REVIEW_CYCLES`, `BOSS_MIN_REVIEW_CHANGED_LINES`, `BOSS_ENABLE_REVISION_TRIGGERED_REVIEWS`) are environment variables read once at startup in `engine/core/src/config.rs` and copied into the completion handler. Changing one needs an engine restart. `settings.toml` (`engine/core/src/settings.rs`) holds per-installation boolean preferences and, since mono#3334, bounded integers that are applied at engine start. Numeric dials that change live (the driver traffic split, the Sol share) are in the `metadata` table and are set through their own requests.

### Finding 2: Boss already runs two A/B experiments, each built from scratch, and they disagree on the unit

- **Just-in-time conflict resolution.** Two flags: `conflict_resolution_deferral` (default off) and `conflict_resolution_deferral_ab_split` (default on). With both on, odd-numbered PRs are treated and even-numbered PRs are control. `ConflictDeferralArm::assign` is a pure function of the two flag values and the PR number, and the arm is written onto each `conflict_resolutions` row "because the flags can change mid-experiment and the analysis must compare what was actually done".
- **The Codex Sol share.** A percentage in `metadata` (`codex_sol_percentage`), a SHA-256 draw on `codex-sol:<execution id>` (`driver_allocation::hash_bucket`), a decision table (`execution_sol_decisions`), a per-family outcome ledger (`sol_pr_outcomes`: baseline head, first head, first CI result, merge time), and a report, `boss cost sol`, that gives each arm's first-attempt CI pass rate, revisions per merged PR, review findings per PR and tokens per merged PR.

One splits by PR and the other by execution. No recorded reason for either choice was found, so this design treats the unit as never decided. The Sol report has to repair its own unit: a PR family "enrolls in the arm of its first launched eligible execution" and later executions are charged to that arm whatever they drew, and its documentation calls the result observational when the mix changes.

### Finding 3: the launch record has three columns and one writer with 39 call sites

`work_executions.driver`, `.model` and `.effort_level` are written once per execution by `record_execution_launch_config` (`engine/work-db/src/execution_launch_config.rs`), after the runner has launched the worker, so the row is "an honest record of what ran". A later launch of the same execution leaves the row as the first launch wrote it. The resolved `SpawnConfig` (`engine/effort/src/lib.rs`) also carries the reasoning mode, which is not stored. The writer takes four positional arguments and has 39 call sites in 20 files. Two are production code (`engine/dispatch/src/coordinator/run.rs` and `progression_spawn.rs`). The rest are tests.

### Finding 4: usage rows cannot tell review roles or revision types apart

The run-grain usage request (`GetTokenUsageRows`, `boss cost rows`, crate `engine/usage-report`) has landed, and so has the app's Token Usage window with its drill-down table (mono#3917). The series request and the over-time tab have not. Rows group by thirteen fixed dimensions (`protocol/src/types/usage_report.rs`). None is an experiment arm. The finest "kind of run" it offers is the execution kind, and:

- pre-merge reviewers, the review supervisor and the post-merge reviewer are all `pr_review`, told apart only by `pr_review_batch_members.role`;
- CI-fix, conflict and review-finding revisions are all `revision_implementation`, told apart only by the `tasks.created_via` prefix.

So the investigation's "by pipeline stage" table cannot be reproduced in the window. `work_runs.rounds` is captured but appears only in JSON output and is never aggregated. There is no first-call context, peak context, compaction count or tool-call count per run.

### Finding 5: tokens per merged PR exists only inside the Sol report

The roll-up the investigation calls "the metric every A/B above needs" (gap 4) is computed by `sol_comparison_records` for Sol-enrolled families and nowhere else.

### Finding 6: the coordinator's tokens are recorded nowhere

Token capture is keyed by execution and run. The coordinator session is neither, and nothing in the engine reads its context size or usage. About 4% of the measured week is therefore invisible to the Token Usage window, and R8's size-based rotation has no signal to act on. No decision to leave the coordinator out was found.

### Finding 7: Claude subagent tokens are not counted

The cost-attribution design's Defect 3 stands: a Claude run's subagent usage is excluded from `work_runs`. Any change that moves work from the main loop into a subagent would look cheaper than it is.

### Finding 8: the remote spawn path reads no feature flags

`remote_spawn_opts` (`engine/core/src/host_adapter.rs`) builds the prompt options for a worker on a remote host without consulting the flag store, so every flag-gated prompt seam is off there. No recorded reason was found.

### Finding 9: premises in the investigation that do not hold on `main`

- **"The same lockfile exclusion as pre-merge prompts" (R1 step 2).** There is none. The pre-merge embed is a raw line count of the whole diff against `BOSS_MAX_EMBED_DIFF_LINES` (`engine/execution-runner/src/runner/worker_spawn.rs`).
- **"The worker rules already forbid watching CI" (R4 step 2).** True for a Standard worker. The CI-remediation brief (`compose_ci_remediation_fragment`, `engine/execution-runner/src/runner/prompt.rs`) instructs `gh pr checks --watch`.
- **The review pipeline it measured is opt-in in the repository.** The two-reviewer batch, and with it the supervisor and post-merge review, run only when `review_batch_fanout` is on; guides only when `review_guide_source_capture` and `review_guide_generation` are on. All three default off. The investigation notes this in its limits. This pass could not see which are on in the installation that was measured.
- **A comment contradicts the code.** `engine/work-db/src/review_batches.rs` says the post-merge reviewer's `large` effort is Claude `high`. The mapping in `engine/driver/src/claude.rs` gives `xhigh`, as the investigation says.

### Finding 10: the app drops a usage reply that names a dimension it does not know

The window decodes the reply's dimensions into a closed list of thirteen (`app-macos/Sources/TokenUsageModels.swift`), and the engine reports observed values for every dimension it has, on every reply. A reply that names a fourteenth fails to decode as a whole, and the window gets nothing. The first change that adds a dimension therefore breaks the landed window unless the same change teaches the app to skip a dimension it does not know.

### Status on `main` (perishable, as of 2026-11-23)

| Item                                      | State on `main`                                                                                                                     | In this design                                            |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| Follow-up 1: Grok tokens per run (gap 2)  | Landed in mono#3799 and mono#3859, with Antigravity. Neither driver reports cache writes.                                           | Closed.                                                   |
| Follow-up 2: quota snapshots (gap 1)      | Not present. An in-memory cache with a 15-minute lifetime (`engine/driver-quota/src/cache.rs`), no table, and dispatch never reads. | Entry "Provider quota snapshot history".                  |
| Follow-up 3: price table (gap 3)          | Stale. Opus at 15 / 75, no Fable entry, one flat cache-write rate (`engine/core/src/cost_pricing.rs`).                              | Entry "Refresh Claude list-price weights…".               |
| Follow-up 3: tokens per merged PR (gap 4) | Only inside `boss cost sol`.                                                                                                        | Entry "PR-family outcomes report per experiment arm".     |
| Follow-up 4: review yield, Deep histogram | Not present. The inputs are stored.                                                                                                 | Entry "Review yield report and Deep-criterion breakdown". |
| Follow-ups 5–14                           | None has landed.                                                                                                                    | The experiments and two straight changes.                 |
| Follow-up 15: waste by reason (gap 11)    | Partial. Usage rows group by execution status. No reason.                                                                           | Entry "Usage end-reason breakdown…".                      |
| Cheaper model policy for progression runs | Landed in mono#3746.                                                                                                                | Not repeated.                                             |
| Usage rows request and `boss cost rows`   | Landed, with the app's table window (mono#3917). The series request and the over-time tab have not.                                 | Readers build on rows. The app entry extends the window.  |

## Alternatives considered

### A. Give each experiment its own mechanism, as the Sol share has

A setting, a set request, a decision table and a report per experiment.

**Rejected.** The Sol share cost a metadata key, a request and event, a decision table, an outcome ledger, two triggers and a report. Nineteen copies of that is the parallel system this project was asked not to build. The Sol share needed its own state for a reason that does not apply here: its draw can be redrawn before launch, and a failed Sol run escalates the work item to another model for good. An on/off treatment with a fixed split has neither property, so its arm can be recomputed from the family id and needs no table.

### B. Split by execution id

The Sol share's unit. About 2,000 or more runs a week against 324 PRs.

**Rejected as the unit, for two reasons that hold for these experiments and did not for Sol.**

- **Several treatments remove runs.** Skipping a re-review, a post-merge review or a guide leaves no execution to draw on. The saving is the absence of a run, and it can only be charged to something that still exists, which is the PR.
- **The outcomes are per PR.** Revisions per PR, findings per PR, first-attempt CI and tokens per merged PR are meaningless for a PR whose implementation run was treated and whose revisions were not. The Sol report shows the cost: it has to enrol each family in its first execution's arm and charge later, differently drawn executions to it.

Both requirements are real ones and not artefacts of the choice: the first comes from R1, R2 and R6 as written, the second from the investigation's gap 4. The price is fewer units, which section 7 puts numbers on. The Sol share's own choice is not disqualified by this reasoning. Its treatment is a per-run model choice that removes nothing.

**A unit chosen per experiment** (the family for the review experiments, the execution for launch-only ones such as the compact window) was also considered, and is not adopted in the first version. Every launch experiment still has a guardrail that is per PR (revisions, review findings, CI-fix revisions), and that guardrail cannot be read when a family's runs are in different arms. Two units would also give the recorded arm set two meanings. The gain would be small where it matters: the launch experiments are read on per-run figures that move a lot (first-call context, peak context), and the one that depends on rare events, the compact window, is limited by the number of very large runs, which the unit does not change. If stage-level reads turn out too slow, this is the first thing to revisit.

### C. Ship every change as a plain kill switch and compare before with after

The 27 registered flags work this way, apart from the conflict-resolution pair.

**Rejected as the default, kept where nothing else is possible.** The measured week itself shows why: Codex went from 2.12B of work to a 0% share inside the window, so any before/after comparison across that boundary measures the mix. A concurrent control arm sees the same mix, volume and repository state as the treatment. Before/after remains the only option for a singleton (the coordinator) and for changes that cannot be served to two arms (the repository instruction file).

### D. A configurable share per experiment

A percentage, as the Sol share has.

**Rejected for the first version.** The variance of a difference between two arms is proportional to `1/n_treatment + 1/n_control`. At a 10/90 split that is 2.8 times the 50/50 figure, so every experiment would take 2.8 times as long to read at a volume where a whole-PR effect already takes weeks. The flag store holds booleans only, so a share would also need a second store or a new value type. The integer settings added in mono#3334 are not that store: they are applied at engine start, so a share kept there could not change without a restart, and a share that changed would have to be recorded with every arm. The Sol share is a rollout dial for a default model and sits at 100%. These are measurements, and a measurement wants equal arms. The kill switch bounds the risk a small share would otherwise bound.

### E. Environment variables or startup configuration

Where two of the relevant knobs already live.

**Rejected.** They are read once at startup (Finding 1), and so are the integer settings in `settings.toml`. An experiment that needs an engine restart to stop cannot be turned off instantly. The same reasoning keeps each treatment's thresholds out of settings: a value that can change under a key breaks the rule that a key names one treatment (section 1).

### F. A separate assignment table

One row per execution and experiment, as `execution_sol_decisions` is.

**Rejected.** It is a second launch record. The usage projection already reads the launch tuple from `work_executions`, and a second place would need a second join and a rule for what happens when the two disagree. The Sol table stores a bucket and a percentage because the percentage varies. With a fixed half-and-half split on a stable family id there is nothing to store that cannot be recomputed, except the one thing that must be stored: the arm that was in force.

## Chosen approach

### 1. The invariant

**An execution's launch record says which arm of each running experiment its PR family was in when it launched, and every figure that compares arms is computed by the engine from that record.**

The load-bearing property is that what was in force for a run can be recovered from the run's own row without knowing the history of the flags. Four things follow:

- **Flags may change at any time.** Analysis never depends on when a flag was flipped.
- **Control is recorded as explicitly as treatment.** A missing key means the experiment was not running for that launch, or nothing was recorded. It never means control.
- **A stop is recorded too.** The row is written at an execution's first launch. If the experiment is turned off before a later run of the same execution, the row is corrected to say so (section 5). It never claims a treatment that one of its runs did not get.
- **A treatment's definition is fixed for the life of its key.** Changing a threshold (a compact window, a line bound) is a new key, so old and new data are never pooled under one name.

### 2. Setting, reading and stopping an experiment

An experiment is one entry in a new registry in `boss-feature-flags` (`key`, optional `lane`, flags it `requires`, and whether it splits) plus flags in the existing registry, category `experiments`:

| Flag                | Default | Effect                                                                                                |
| ------------------- | ------- | ----------------------------------------------------------------------------------------------------- |
| `<key>`             | off     | The experiment's own switch. Off: every family gets today's behaviour and nothing is recorded for it. |
| `<key>_ab_split`    | on      | On: half of families are treated. Off: every family is treated.                                       |
| `token_experiments` | on      | Master switch. Off: every experiment resolves to off at once.                                         |

This is the just-in-time conflict resolution pair, generalised, with a master switch of the kind `worker_proposals` already is for the proposal seams. Three states result: off, split, all.

- **Set.** Debug → Feature Flags in the app. No new request, verb or UI.
- **Read.** On every launch and every policy decision, through the store's live `is_enabled`. Nothing is cached.
- **Stop.** Turn `<key>` off. The next launch and the next policy decision get today's behaviour. A worker already running keeps the brief and launch flags it started with, because they cannot be withdrawn. If the fault is in the launch itself, cancel the treated runs still in flight.
- **Ships inert.** Every experiment merges with `<key>` off. Each experiment PR says so in its description, and names the flag that makes it live.

The feature-flags README's safety contract is "Default ON unless the flag is known-risky", and it calls a default-off flag a manual opt-in. In practice 23 of the 27 registered flags default off. An experiment key is a manual opt-in by design, and the README is amended in the first entry to describe experiment flags as their own case.

### 3. Assignment: half of PR families, by a stable hash

- **Unit.** The PR family: the chain root work item and everything under it. This is the root the review cycle already counts on and the family `boss cost sol` enrols. An execution with no task row uses its own `work_item_id`, except a review guide, which uses the family of the PR it explains when that resolves.
- **Draw.** `hash_bucket("experiment:<key>:<family root id>")`, the SHA-256 bucket in `[0, 100)` that the driver split and the Sol share already use. Below 50 is treatment. Including the key makes two experiments' assignments independent of each other.
- **Purity of the function.** As with `ConflictDeferralArm::assign`, the flags crate takes the flag values and a bucket and returns an arm. The work database computes the bucket. The flags crate gains no dependency.
- **Stable.** The same family gets the same arm for as long as the flags stay put, at every launch and at every policy decision.
- **Requirements.** An experiment whose required flag is off (for example `delta_rereview` without `review_batch_fanout`) resolves to off and records nothing.

### 4. Lanes: what may not run together

Two experiments that rewrite the same instructions, or that reshape the same review, cannot be told apart by any amount of careful assignment, because the treated run received both. A lane is a named surface on which only one experiment may be assigned at a time:

| Lane                  | Surface                                                         |
| --------------------- | --------------------------------------------------------------- |
| `worker_prompt`       | The rules and brief text a Standard worker is given.            |
| `pre_merge_review`    | Which pre-merge reviews run, their shape, and what they mint.   |
| `post_merge_review`   | Whether a post-merge review runs, its shape, and what it mints. |
| `coordinator_context` | How the coordinator's context is bounded.                       |

**The rule is enforced, not advised.** If two experiments in one lane are switched on, the one registered first runs and the other resolves to off. `boss cost experiment` lists it as held and names the experiment holding the lane.

Experiments with no lane (a launch flag, a tool guard, a model choice for one role) may overlap with anything. They are kept apart by independent assignment: each one's arms are balanced across the other's, so its effect is read with the other averaged out, and the recorded arm sets let a suspicious result be re-read inside the other's control arm.

**Starting context is the one case that needs saying out loud.** `claude_lean_environment` and `slim_worker_brief` both shrink what a worker starts with, and only the second is in the prompt lane. They act on disjoint parts (what the CLI loads from the personal environment, and what the engine composes), and the figure they are read on, first-call context per run, is a sum of those parts, so each arm difference is measured directly whatever the other is doing.

### 5. Recording: one more column in the launch record

- **Column.** `work_executions.experiment_arms`, text: a JSON object from experiment key to `control`, `treatment` or `stopped`, keys sorted. `{}` means recorded and nothing was running. NULL means not recorded (before the migration).
- **Writer.** One function that takes a struct (driver, model, effort, arm set) and writes them in one transaction. The existing four-argument function stays as a wrapper so its 39 call sites do not change.
- **When.** At the same moment as the launch tuple, once per execution, from the arm set the runner resolved before it composed the prompt.
- **What is recorded.** The family's arm for every experiment running at that launch, whether or not this run's own launch differs. A Grok run in a family treated by a Claude-only experiment records `treatment`. Each experiment's operator-doc row names the filters that isolate the runs it changes.
- **Sticky.** A later run of the same execution reuses the stored set, so the record stays true across a retry. An experiment that started after the first launch is not picked up by a later run.
- **A stop rewrites the record.** With `<key>` off, a later run of an execution that recorded the key launches untreated, and the writer changes that key's value to `stopped`, whichever arm it held. Control is rewritten as well as treatment so that both arms lose such executions alike. This is the only update the writer makes after the first launch.
- **Knobs do not know about experiments.** The runner turns the arm set into plain options (a composer parameter, a launch-options value handed to the driver through a defaulted trait method). Drivers and the composer never see an experiment key, so adopting a treatment later is a change of a default.
- **Policy decisions.** Code that decides something for a family (skip a re-review, admit a post-merge review) calls the same resolver with the same family id. Where the caller cannot reach the flag store (the merge poller, the work database) the arm is passed in, as the conflict-deferral arm is today.

### 6. Reading an experiment

**Usage rows (entry "Usage dimensions for experiment arm and pipeline stage").**

- A new dimension `experiment_arm`, and a new optional request field `experiment` naming which experiment it refers to. With the field set, each run has one value: `treatment`, `control`, `stopped`, `not_enrolled` or `not_recorded`. Without it, the value is the whole recorded set as one string, which shows every combination when two experiments overlap.
- The reply lists the experiments observed in the window with run counts and family counts per arm, so a picker is built from data and a new experiment needs no app change.
- A new dimension `pipeline_stage`, classified by the engine from the execution kind, the review-batch member role and the task's provenance: filed work, review-finding revision, CI-fix revision, conflict revision, progression-minted work, post-merge follow-up, pre-merge review, review supervisor, post-merge review, review guide, and the remaining kinds by name. This is the investigation's stage table as a dimension.
- Each run appears under exactly one value of each dimension, so group sums still add up to the window total and the app still adds nothing itself.
- The standard read is `boss cost rows --since 14d --experiment <key> --group-by experiment_arm,pipeline_stage`, with a filter on the stage or driver the experiment changes.

**Per-run shape (entries "Per-run context shape…" and "Per-run tool-call classes…").** First-call context, peak context, compactions, rounds per run, tokens per round, and calls per run by class, per run and as engine-computed figures on every group. These are the mechanism checks: they show that a treatment did what it claims before anyone argues about the total.

**Family outcomes (entry "PR-family outcomes report per experiment arm").** `boss cost experiment <key>` generalises `boss cost sol`. A family enrols at its first launched execution that records the key. A family in which another execution records a different arm or none, or any execution records `stopped`, is mixed: it is left out of the comparison and counted. Per arm: families, merged families, tokens per merged family, first-attempt CI pass rate, revisions per merged PR, review batches per PR, review findings per PR and post-merge follow-ups per merged PR. This is where the guardrails are read.

**Review yield (entry "Review yield report and Deep-criterion breakdown").** Findings by severity and category per review round and role, what passed the revision gate, and whether the resulting revision or follow-up changed code and merged. This is the guardrail for every review experiment and the "pull yield first" step the investigation asks for before R1 and R2.

### 7. How long an experiment needs

A back-of-envelope figure, with its assumptions stated, because it decides how the experiments are read.

- The measured week had 324 PRs, so a half-and-half split gives about 160 families per arm per week.
- To see a relative difference `d` between two arm means at the usual 80% power and 5% significance needs about `16 × CV² / d²` units per arm, where `CV` is the standard deviation divided by the mean.
- Tokens per PR are heavy-tailed (ten runs used 791M). The spread was not measured. **Assuming** `CV = 1.5`, a 30% difference needs about 400 families per arm (two and a half weeks), a 20% difference about 900 (five and a half weeks), and a 10% difference about 3,600 (five months).

Most recommendations are estimated at 2–8% of the total. **None of them can be read on whole-PR tokens in a reasonable time.** Each can be read where its effect is concentrated: `post_merge_review_slim` is expected to take a post-merge review from 5.58M to about 2.5M, a 55% change on about 73 runs per arm per week, which by the same formula is visible within a week even at `CV = 1`. That is why the pipeline-stage dimension and the per-run shape figures come first, why each experiment below names a primary figure on its own stage, and why family-level outcomes serve as guardrails and not as the measure of saving.

**Runs in one family are not independent.** They share an arm and they resemble each other (the same change, the same files), so for a figure read per run the effective sample lies between the number of families and the number of runs. Post-merge review has about one run per family, so the example above stands. A worker-run figure such as rounds per run is affected, which is why every arm comparison reports families beside runs.

Every experiment here is a choice between two options: today's behaviour and one treatment. None can show that a third option is better. Where the investigation offered alternatives, the tables below say which one is tested.

### 8. What is not an experiment, and why

| Item                                              | Why it cannot be an experiment                                                                                              | What it becomes                                                                               |
| ------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| Follow-up 1, gap 2: Grok tokens                   | Instrumentation. Already landed.                                                                                            | Closed.                                                                                       |
| Follow-up 2, gap 1: quota snapshots               | Instrumentation. It observes and changes no run.                                                                            | Entry "Provider quota snapshot history".                                                      |
| Follow-up 3, gap 3: price table                   | Instrumentation.                                                                                                            | Entry "Refresh Claude list-price weights and price cache writes by lifetime".                 |
| Follow-up 3, gap 4: tokens per merged PR          | Instrumentation.                                                                                                            | Entry "PR-family outcomes report per experiment arm".                                         |
| Follow-up 4, gaps 5 and 6: review yield           | Instrumentation.                                                                                                            | Entry "Review yield report and Deep-criterion breakdown".                                     |
| Gap 7: calls and context by category              | Instrumentation.                                                                                                            | Entry "Per-run context shape…".                                                               |
| Gaps 8 and 9, R4 step 4: waits, polls, tool sizes | Instrumentation.                                                                                                            | Entry "Per-run tool-call classes…".                                                           |
| Gap 10: itemise the fixed context                 | Needs `/context` in a live worker session. Nothing in the repository can produce it.                                        | An owner action. The context-shape entry records first-call context per run.                  |
| Gap 12: stale forensic doc                        | A documentation correction.                                                                                                 | Folded into the context-shape entry.                                                          |
| R10, follow-up 15, gap 11: wasted runs            | A defect has no control arm worth keeping, and nothing can be fixed until the waste is broken down by reason.               | Entry "Usage end-reason breakdown and overlapping-execution caveat". Fixes are filed from it. |
| R3 step 4: slim the repository instruction file   | The agent CLI loads this tracked file from the checkout. Two arms cannot see two versions without dirtying a worker's tree. | A straight change, read before and after.                                                     |
| R5 step 3: runaway-run alarm                      | An alarm changes no run, so there is no arm to compare.                                                                     | A straight change behind a kill switch.                                                       |
| R8: the coordinator                               | One coordinator at a time. There is no concurrent control.                                                                  | Flag-controlled, all or nothing, compared week against week.                                  |

One part of R10 can be answered from the documentation now. An orphaned run's successor does not start from nothing: unpublished work is preserved once at teardown under a `preserved/<execution>` bookmark and restaged on resume (`docs/post-crash-recovery.md`). What is lost is the conversation. Whether a written summary of the lost run would pay cannot be said until the breakdown by reason exists.

**Conditional steps, recorded and not filed.** The investigation words three steps as "consider" or "if". Each depends on an observation that can come out either way, so an entry filed now could not be acted on. Each is filed when its condition is observed, as an experiment where the last column says so.

| Step                                                | The investigation's condition                                                                 | What decides it                                                                         | If the condition holds                                                                                                |
| --------------------------------------------------- | --------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| R2 step 4: a review cap of two rounds               | "Once yield per round is known, consider a cap of 2 rounds."                                  | The review yield report: findings that led to a merged fix, by round.                   | An experiment in lane `pre_merge_review` that lowers the cap for treated families at the existing cap check.          |
| R9: keep engine-minted revisions off the Sol model  | "If Codex is put back into the split before R4 lands." Codex's share of new work was 0%.      | The driver traffic split, and whether `blocking_waits` has been adopted by then.        | A change to Sol eligibility in `engine/effort/src/lib.rs`. `boss cost sol` already reports Sol's cost per merged PR.  |
| R8 step 3: batch the coordinator's inbound messages | "Measure how many of the 1,719 calls were inbound messages, and batch them if they dominate." | The share of coordinator turns that begin with an inbound message, once it is recorded. | Batched delivery in `engine/core/src/coordinator_tmux/inbox.rs` behind an all-or-nothing flag, compared week on week. |

Three other steps are recommended without a condition and only wait on something else: the exploration subagent (R7 step 3), the on-demand crate map (R7 step 4) and the single-call review guide (R6). They are deferred entries in the breakdown.

### 9. The experiments

What each one changes. Savings are the investigation's weekly estimates at the measured mix; they overlap and must not be added. "Weighted" is how the saving looks at list price against raw tokens.

| Key                           | From         | Lane                  | Runs it changes                      | Treatment                                                                                                                           | Est. saving                                                          | Weighted | Risk          |
| ----------------------------- | ------------ | --------------------- | ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- | -------- | ------------- |
| `post_merge_review_slim`      | R1 steps 1–2 | `post_merge_review`   | Post-merge reviewer                  | `medium` effort, the merged diff embedded, and no post-merge review of a follow-up that a post-merge review minted.                 | ≈520M for step 2. 6.4M per avoided review for step 1.                | same     | medium        |
| `post_merge_size_eligibility` | R1 step 3    | `post_merge_review`   | Post-merge admission                 | Eligible on size (over 1,000 lines or 25 files), or on two complexity flags plus at least 400 changed lines.                        | ≈440M if eligibility halves.                                         | same     | medium        |
| `post_merge_followup_bar`     | R1 step 4    | `post_merge_review`   | Post-merge verdicts                  | Only a critical or high finding mints a follow-up task.                                                                             | Not estimated.                                                       | same     | medium        |
| `delta_rereview`              | R2 step 1    | `pre_merge_review`    | Review rounds after the first        | One Light-tier reviewer, no supervisor, a brief with the prior findings and the delta, and one explicit whole-PR duplication check. | ≈470M.                                                               | same     | medium        |
| `revision_gate_bar`           | R2 step 3    | `pre_merge_review`    | Pre-merge verdicts                   | The four always-on categories need medium severity to mint a revision. Lower findings are posted as one PR comment.                 | ≈290M per 10% of finding revisions avoided. The share is unmeasured. | same     | medium        |
| `min_review_changed_lines`    | R2 step 2    | `pre_merge_review`    | Re-review of small pushes            | A revision push changing fewer than 10 lines since the last reviewed commit is not re-reviewed.                                     | Not separately estimated.                                            | same     | medium        |
| `claude_lean_environment`     | R3 step 1    | none                  | Every Claude worker                  | No skills, no inherited MCP servers, unused built-in tools removed, the user-level instruction file excluded.                       | ≈200M measured, up to ≈370M more if tool definitions go.             | same     | low           |
| `slim_worker_brief`           | R3 steps 2–3 | `worker_prompt`       | Standard workers, all drivers        | Each rule stated once. Kind-specific text only for the kinds that can use it.                                                       | ≈95M.                                                                | same     | low           |
| `blocking_waits`              | R4 steps 1,3 | `worker_prompt`       | Standard workers on Claude and Codex | Claude: background, declare the wait, end the turn. Codex: a longer background-terminal timeout and a matching yield.               | Claude ≈110M with the guard below. Codex ≈470M at the measured mix.  | weaker   | low           |
| `ci_watch_guard`              | R4 step 2    | none                  | Claude Standard workers              | The tool guard refuses CI watch commands.                                                                                           | Part of the Claude ≈110M.                                            | weaker   | low           |
| `worker_autocompact`          | R5 steps 1–2 | none                  | Claude workers                       | A 200k auto-compact window, and compact instructions in the worker rules.                                                           | ≈0.4–0.5B.                                                           | weaker   | low to medium |
| `review_guide_when_ready`     | R6           | none                  | Review guide runs                    | One guide when the PR is ready for a person or first opened, at `small` effort (Codex `medium`).                                    | ≈270M.                                                               | same     | low           |
| `ranged_reads`                | R7 step 1    | none                  | Claude Standard workers              | The tool guard refuses a whole-file read of a file over 350 lines. A lower read-output cap as a backstop.                           | ≈180M.                                                               | stronger | medium        |
| `batched_search`              | R7 step 2    | `worker_prompt`       | Standard workers, all drivers        | The brief tells workers to issue independent searches and reads in one turn.                                                        | Part of 0.3–0.6B the investigation labels experimental.              | stronger | low           |
| `coordinator_autocompact`     | R8 step 1    | `coordinator_context` | The coordinator                      | A 150k auto-compact window.                                                                                                         | ≈140M.                                                               | weaker   | low to medium |
| `coordinator_size_rotation`   | R8 step 2    | `coordinator_context` | The coordinator                      | Rotate to a fresh session through the handoff when context passes 150k.                                                             | The same budget by another route.                                    | weaker   | low to medium |
| `supervisor_standard_tier`    | R9           | none                  | Supervisor in Deep batches           | Sonnet whatever the batch profile.                                                                                                  | Tokens about unchanged. 83M moves off Opus.                          | stronger | medium        |
| `ci_fix_standard_tier`        | R9           | none                  | CI-fix revisions with no evidence    | Standard reasoning at `medium` effort instead of the investigation tier.                                                            | Bounded by the 138M CI-fix line.                                     | stronger | medium        |
| `investigation_routing`       | R9           | none                  | Claude design and investigation runs | `large` effort launches at Claude `high` instead of `xhigh`. `small` and `medium` launch on Opus instead of Fable.                  | Within R9's 0.1–0.3B.                                                | stronger | medium        |

How each one is read, and what stops it. "Rows" is `boss cost rows --experiment <key> --group-by experiment_arm` with the filter shown; the Token Usage window offers the same grouping once the app entry lands. "Outcomes" is `boss cost experiment <key>`. "Yield" is `boss cost review-yield --experiment <key>`.

| Key                           | What shows the effect                                                                                           | Guardrail                                                                                          | Stop early when                                                                                                    |
| ----------------------------- | --------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| `post_merge_review_slim`      | Rows, stage `post_merge_review`: runs and tokens per run.                                                       | Yield: critical and high findings per review, and the share of follow-ups that merge.              | Treated reviews find fewer than half the critical and high findings of control.                                    |
| `post_merge_size_eligibility` | Rows, stage `post_merge_review`: runs. Outcomes: post-merge follow-ups per merged PR.                           | Yield, control arm: merged critical and high follow-ups from PRs the treatment would exclude.      | Excluded PRs yield merged critical or high fixes at the rate kept PRs do.                                          |
| `post_merge_followup_bar`     | Rows, stage `post_merge_followup`: runs and tokens.                                                             | Yield, control arm: suppressed-severity findings whose follow-ups merged.                          | Most follow-ups from findings below the bar merge in control.                                                      |
| `delta_rereview`              | Rows, stages `pre_merge_review` and `review_supervisor`. Outcomes: review batches and tokens per merged family. | Outcomes and yield: post-merge critical and high findings per merged PR, and duplication findings. | Post-merge critical and high findings per merged PR exceed 1.5 times control.                                      |
| `revision_gate_bar`           | Rows, stage `review_finding_revision`: runs. Outcomes: revisions per merged PR.                                 | Yield: post-merge findings per merged PR in the four categories.                                   | The same 1.5 times rule, on those categories.                                                                      |
| `min_review_changed_lines`    | Rows, stage `pre_merge_review`: runs. Outcomes: review batches per PR.                                          | Outcomes: first-attempt CI pass rate and post-merge findings per merged PR.                        | The same 1.5 times rule.                                                                                           |
| `claude_lean_environment`     | Rows, driver `claude`: first-call context and first-call cache write per run.                                   | Every treated run still delivers hook events. Failed-to-start and blocked rates.                   | Any treated run that starts without hooks or fails on a launch flag. Blocked or failed rate 5 points over control. |
| `slim_worker_brief`           | Rows, non-review stages: first-call context per run.                                                            | Runs ending with no PR or no done declaration. Pushes refused by the check gate. Blocked rate.     | No-PR or undeclared rate 5 points over control.                                                                    |
| `blocking_waits`              | Rows by driver: rounds per run and tokens per run. Poll and wait calls per run.                                 | Stale-worker reaps, expired waits, and execution duration.                                         | Any treated run stalls on a wait that never wakes. Median duration up by a quarter.                                |
| `ci_watch_guard`              | Rows, driver `claude`: rounds per run. CI-watch calls per run.                                                  | Runs ending blocked after a refusal.                                                               | Any run ends blocked citing the refusal.                                                                           |
| `worker_autocompact`          | Rows, driver `claude`: peak context, runs compacted, tokens per run.                                            | End-status mix of compacted runs, and the share that ended without a done declaration.             | A compacted run loses its task or its done declaration. Undeclared rate twice control.                             |
| `review_guide_when_ready`     | Rows, stage `review_guide`: runs and tokens.                                                                    | Opens that found no current guide, and the wait from open to guide (Metrics window).               | The owner keeps opening PRs with no guide.                                                                         |
| `ranged_reads`                | Rows, driver `claude`: tokens per run. Read-result size per run.                                                | Rounds per run must not rise. Outcomes: review findings and CI-fix revisions per PR.               | Rounds per run up 15% with tokens per run no lower.                                                                |
| `batched_search`              | Rows, non-review stages: rounds per run and tokens per run.                                                     | Outcomes: review findings per PR.                                                                  | None beyond the general rule below.                                                                                |
| `coordinator_autocompact`     | Rows, stage `coordinator_session`: tokens per day, tokens per round, compactions.                               | A manual read of the handoff after a compaction. Operator resets per week.                         | The coordinator loses a decision it had made.                                                                      |
| `coordinator_size_rotation`   | The same.                                                                                                       | The same, and rotations per day.                                                                   | More than a few rotations a day.                                                                                   |
| `supervisor_standard_tier`    | Rows, stage `review_supervisor`, by model.                                                                      | Yield: consolidated findings per batch against the leaf reviewers' findings.                       | Consolidated findings per batch fall by a third at the same leaf findings.                                         |
| `ci_fix_standard_tier`        | Rows, stage `ci_fix_revision`, by model: tokens per run.                                                        | Outcomes: repeat CI-fix revisions per PR.                                                          | Repeat CI-fix revisions per PR exceed 1.5 times control.                                                           |
| `investigation_routing`       | Rows, task kind investigation or design, by effort and model: output tokens and tokens per run.                 | Blocked and failed rate. Document revisions per run. The owner's reading of the documents.         | The owner judges the documents worse.                                                                              |

Rules that apply to all of them:

- **A hard failure stops an experiment at once.** A treated run that cannot start, loses its hooks, or ends in a state the engine cannot classify is a reason to turn the key off without waiting for a count.
- **A rate guardrail is read once each arm has 40 units** (families, or affected runs for a run-grain rate). Before that only the hard-failure rule applies.
- **The stop conditions act during the experiment, through the kill switch.** They are not a gate at the end of it.
- **Launch behaviour is proved by real runs.** The launch-line goldens pin a string. Whether the CLI honours a flag, keeps firing hooks, or wakes a session when a background command exits is only shown by the first treated runs. Those claims come from vendor documentation as the investigation quotes it and were not checked against the installed CLI in this pass.

### 10. Ordering and concurrency

**Build order.**

1. **First, in parallel:** the experiment mechanism, per-run context shape, quota snapshot history, the price weights and the runaway-run alarm. None depends on another. The review yield report follows the mechanism, because it splits by the recorded arm.
2. **Then the readers:** the usage dimensions and the family outcomes report, then the app picker.
3. **Then the experiments,** in the investigation's rank order where they do not share files, and in chains where they do (the breakdown names each chain).
4. **Last:** tool-call classes, the end-reason breakdown, coordinator usage and the coordinator experiments. They serve the lowest-ranked recommendations and sit at the end of the usage-rows file chain.

Experiment entries merge inert, so they may merge before the readers do. **No experiment is turned on until the usage dimensions and the outcomes report have landed,** and an experiment whose primary figure comes from a capture (first-call context, peak context, tool-call classes) is not turned on until that capture is live, because a run that launched before it has nothing to show.

**Enable order.** Recommended waves, by estimated saving and risk, one experiment per lane and at most four at once:

| Wave | Experiments                                                                                                                           |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | `post_merge_review_slim` (R1), `delta_rereview` (R2), `claude_lean_environment` and `slim_worker_brief` (R3).                         |
| 2    | `post_merge_size_eligibility` (R1), `revision_gate_bar` (R2), `blocking_waits` (R4), `worker_autocompact` (R5).                       |
| 3    | `post_merge_followup_bar` (R1), `min_review_changed_lines` (R2), `review_guide_when_ready` (R6), `ranged_reads` (R7).                 |
| 4    | `batched_search` (R7), `ci_watch_guard` (R4), and the three routing experiments (R9), counted as one because their runs are disjoint. |
| Any  | The coordinator pair (R8), one at a time, once coordinator usage is recorded. It touches no PR family.                                |

**Why four.** The fall-back read for a surprising result is to look at one experiment inside the control arms of the others. With `k` experiments overlapping that leaves `324 / 2^k` families per arm a week: 20 at four, 5 at six. Four keeps the fall-back usable inside two weeks. The engine enforces only the lane rule; the cap is guidance.

**The two review lanes share an instrument.** Post-merge findings are the escaped-defect guardrail for the pre-merge experiments, and the post-merge experiments change how often and how hard post-merge review looks. Independent assignment keeps the comparison between pre-merge arms unbiased, because each has the same share of slimmed post-merge reviews. It does make the guardrail less sensitive. If a pre-merge guardrail is close to its line, read it inside the post-merge experiment's control arm.

### 11. Adopting or dropping a treatment

- **Adopt.** One change sets `<key>` default on and `<key>_ab_split` default off. Every family is treated and the kill switch remains. A later cleanup removes the control path, the flags and the registry entry.
- **Drop.** One change removes the treated path, the flags and the registry entry.
- **Sweep the tests in the same change.** The prompt tests pin today's text by substring (for example the "FOREGROUND" instruction). An experiment adds treated variants and leaves those assertions alone. The change that adopts a treatment removes or rewrites the tests that pinned the superseded text, so a retired instruction is not left defended by a test.
- **Keys are never reused,** adopted or dropped.

### 12. Coordination with the Token Usage window and cost-attribution designs

These are edges across projects. Whoever schedules both has to add them.

- **"Record reasoning in the execution launch tuple"** (deferred in the Token Usage window design) and the experiment mechanism here both extend the launch record and both edit `execution_launch_config.rs`. They must not run in parallel. Whichever is second adds its field to the writer struct the first introduced.
- **The usage series request** (not landed) should group through the same classifier as rows and accept the `experiment` field, so arms and stages reach the over-time charts without a second change. If it lands before the usage-dimensions entry here, that entry adds the field to it.
- **The Token Usage window's table entry** landed in mono#3917, so the app entry here has no edge into that project. Grouping by arm in the window is an entry of this project, not an addition to that one.
- **The over-time tab** (not landed) gets arms and stages from the series request's fields. This project files no app entry for it. If the owner wants arm lines on the over-time charts, that is an addition to the Token Usage window project's over-time entry, for the coordinator to file there.
- **Claude subagent transcript enumeration** (cost-attribution design, entry 9) gates the deferred exploration-subagent experiment.
- **The cost-attribution rate table** (that design's entries 13 and 14) replaces `cost_pricing.rs`, the file the price-weights entry here edits. This design keeps its entry: it corrects values in the existing table and needs nothing from that project. If both are scheduled, the price-weights entry goes first and the rate table takes its values and sources over.

## Risks / open questions

- **The unit costs power.** 324 families a week is a small population. Section 7's figures rest on an assumed spread that should be replaced by the measured one as soon as the outcomes report exists. If the spread is much worse than assumed, even stage-level reads slow down and the case for before/after on the smallest experiments gets stronger.
- **A flag flip mixes families in flight.** A family that launched under one arm and later launches under another, or none, is excluded and counted. Flipping a split on and off repeatedly throws away most of a week. The operator doc should say so plainly.
- **The record is per execution, not per run.** Stickiness keeps it true across retries. A kill switch between two runs of one execution is the one case where a run's behaviour differs from what the first launch recorded. The writer marks that execution `stopped`, which takes it and its family out of the comparison in both arms. The tokens its treated first run used are then counted in neither arm.
- **Runs within a family are correlated** (section 7). Counting runs overstates how much evidence a worker-run figure carries. The reports give family counts beside run counts, and nothing in the engine corrects for it.
- **Task creation refuses a breakdown of more than 30 entries, whole** (`DEFAULT_MAX_TASKS` in `engine/core/src/populator.rs`, deferred entries included). This breakdown has 28. A revision that adds three entries has to merge or drop others.
- **Policy experiments are charged through the runs that remain.** A skipped review has no row. Its saving shows as fewer runs in the treated arm, which only reads cleanly while the arm stays put.
- **The prompt lane is slow.** Four prompt experiments at two weeks each is two months. The alternative, overlapping prompt changes, would produce results nobody could attribute. The owner may shorten it by adopting early on a strong mechanism check.
- **Most launch experiments are Claude-only, and Claude is 45% of new work.** The investigation's dispatch split at the end of its window was Grok 55%, Claude 45%, Codex 0%, and Grok's estimated input per run was higher than a Sonnet run's. Grok tokens are now recorded, so Grok can be measured, but the investigation recommends nothing for it because it could not measure it then. That is the largest uncovered area and it is outside this project's scope.
- **`blocking_waits` has no Codex traffic.** At a 0% Codex share its Codex half ships and does nothing.
- **Remote workers are never enrolled** (Finding 8). If remote hosts take a meaningful share of work, half-and-half is no longer half of everything.
- **The review experiments need flags that default off.** If `review_batch_fanout` is off in an installation, the pre-merge and post-merge experiments resolve to off there.
- **`boss cost rows` caps a reply at 5,000 runs,** after filters. An unfiltered two-week window is near the cap, and a capped reply covers the most recent runs only. Experiment reads should filter to the stage they care about. The usage-dimensions entry makes a capped arm comparison say that it is capped.
- **Thresholds chosen here without data:** 10 lines for `min_review_changed_lines`, the 400-line floor for `post_merge_size_eligibility`, 350 lines for `ranged_reads`, 150k for the coordinator, and 25M tokens or 150 rounds for the alarm. Each is the investigation's figure where it gave one and this design's choice where it did not. The yield report and the context-shape capture will show whether they are right, and a different value is a new key.
- **Three alternatives the investigation offered stay untested:** post-merge eligibility by size alone, fetching the failing log before a CI-fix revision, and the two guide triggers separately.
- **The CI-remediation brief instructs a CI watch** (Finding 9). The guard exempts it. Whether that brief should wait on CI at all is not decided here.
- **The price-weights entry overlaps the cost-attribution rate table.** It is kept here because the "weighted" reading of the compaction and reading experiments depends on Fable being priced at all and on Opus not being priced at nearly four times its list price, and because it is a small change to one table. The other reading is that prices belong to the cost-attribution project alone. That is the owner's call, and the questions manifest asks it.
- **Coordinator comparisons are week against week,** with no control, on a session whose load varies. Only a large effect will show.
- **One lockfile diff reached a review brief despite the line cap** (mono#3594, noted by the investigation). It was not traced here and no entry traces it.
- **Progression-minted chores were seen on Fable** in the audit although they are created without a reasoning mode. The pipeline-stage dimension separates progression-minted work, so the first read of rows by stage and model answers whether that still happens after mono#3746.

## Proposed implementation task breakdown

Breakdown size: 28 entries (25 in-scope, 3 deferred) — ten recommendations become 19 experiments, one shipped with the mechanism and 18 packed into 12 experiment entries wherever two or three share one seam and one file set, plus two straight changes, four entries for the mechanism and its readers (launch record, usage dimensions, outcomes report, app picker) and seven instrumentation entries for the gaps the experiments are read through; three deferred entries are experiments the investigation recommends outright that wait on another project's capture or on an earlier experiment's result.

Parallelism. With no dependencies and no shared files beyond append-only registries, these start together: the experiment mechanism, per-run context shape, quota snapshot history, the price weights and the runaway-run alarm. After the mechanism lands, the review yield report, the outcomes report and the first entry of each experiment chain start together. Four chains are ordered because their entries substantially co-edit the same files, and each later entry must forward-port the earlier one's changes preservingly (integrate, never delete):

- **Usage rows chain** (the projection maps columns by position, and every entry adds fields to the same record, classifier, fixture and wire type): per-run context shape, then usage dimensions, then tool-call classes, then the end-reason breakdown, then coordinator usage.
- **Claude launch chain** (`engine/driver/src/claude.rs`, the launch-options value and the launch goldens): auto-compact, then the lean environment, then the tool guards.
- **Worker prompt chain** (`runner/prompt.rs`, the worker rules template and the prompt tests): slim rules and brief, then blocking waits.
- **Review chains** (the post-merge trigger and verdict gate; the pre-merge trigger sites and gate): slim post-merge review, then post-merge gating; delta re-review, then the revision bar and changed-lines skip.

Every experiment entry also appends to the flag registry, the experiment registry and the operator doc, and every migration appends to the migration registry and schema assertions. Those overlaps are incidental and do not serialise anything. The two review chains, the routing entry and the guide entry touch different functions of `review_batches.rs` and `worker_spawn.rs` and may run in parallel.

### Experiment registry, PR-family assignment and arm recording in the launch record

Scope description: Add the experiment mechanism and its first consumer. In `boss-feature-flags`: the experiment registry (key, lane, required flags, whether it splits), the arm type, the pure assignment from flag values and a bucket, lane resolution, the `token_experiments` master switch, and the canonical arm-set encoding. In the work database: the family-root and bucket helpers (reusing `driver_allocation::hash_bucket`), the `work_executions.experiment_arms` column, and one launch-record writer that takes a struct and writes driver, model, effort and arms in one transaction, with the four-argument function kept as a wrapper. In the runner: resolve the arm set before composing, reuse a stored set for later runs of the same execution, rewrite a stored key to `stopped` when it has been turned off by then, pass the set through `WorkerSpawnOpts`, and carry it on `SpawnConfig` to the dispatch site that stamps the launch record and the `pane_spawned` event. The arm set is not added to the protocol's `WorkExecution` type: readers get it from the usage projection, which keeps this entry clear of the construction sites of that type. The first experiment is `batched_search` (R7 step 2, lane `worker_prompt`): treated Standard workers are told to issue independent searches and reads in one turn. Ships inert. To toggle it, use Debug → Feature Flags, section `experiments`: `batched_search` on starts a half-and-half split, `batched_search_ab_split` off treats every family, `batched_search` off stops it. To read it, `boss cost rows --experiment batched_search --group-by experiment_arm` (after the usage-dimensions entry) shows rounds per run and tokens per run by arm.

Major deliverables:

1. Registry, assignment, lanes and master switch in `boss-feature-flags`, with tests pinning off, control and treatment from the flag values, the lane rule, required flags, stable assignment for a fixed family id, and the arm-set round trip.
2. `experiment_arms` in the launch record: migration, single writer, stickiness across runs of one execution, the `stopped` rewrite in both arms when a key is turned off between runs, the arm set on the `pane_spawned` event, and an empty set on the remote spawn path.
3. `batched_search` as the first experiment, the operator doc `docs/token-experiments.md`, and the experiment section of the feature-flags README.

Likely changed files:

- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs` (new)
- `tools/boss/engine/feature-flags/README.md`
- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/execution_launch_config.rs`
- `tools/boss/engine/work-db/src/execution_launch_config_tests.rs`
- `tools/boss/engine/work-db/src/chain_helpers.rs`
- `tools/boss/engine/work-db/src/driver_allocation.rs`
- `tools/boss/engine/effort/src/lib.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/compose_prompt_tests.rs`
- `tools/boss/engine/dispatch/src/coordinator/run.rs`
- `tools/boss/engine/dispatch/src/coordinator/progression_spawn.rs`
- `tools/boss/docs/token-experiments.md` (new)

Estimated size: ~19 files / ~1300 changed lines

Effort: large

Dependencies: none

Scope: in-scope

### Per-run context shape: first-call context, peak context and compactions in usage rows

Scope description: Record per run, from the same transcript pass that captures usage, the context size and cache-write size of the first model call, the peak context, and the number of compactions. The value is NULL where a driver's transcript does not expose it: Claude at minimum, Grok and Antigravity where the turn records carry it, Codex where the rollout does. Surface them per run in the usage rows reply and as engine-computed call statistics on every node and group (runs with rounds, rounds per run, tokens per round, median and maximum peak context, runs compacted, median first-call context), and print rounds, peak context and compactions in `boss cost rows` text output. Correct the documentation that still says per-task cost is not in the database (gap 12). Not an experiment: it observes runs and changes none. To read it, `boss cost rows --group-by execution_kind` shows the new columns. This is the baseline for the starting-context and compaction experiments, which have nothing to show for runs that launched before it.

Major deliverables:

1. Capture and storage: four nullable `work_runs` columns written with the cost snapshot, with tests per driver dialect and for a compacted Claude transcript.
2. Usage rows: per-run fields and per-group call statistics computed by the engine, with a ratio absent when its denominator is zero and never shown as zero, and the `boss cost rows` text columns.
3. Documentation: `docs/run-token-usage.md` and the stale cost statement in `docs/forensic-surfaces.md`.

Likely changed files:

- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/run_rows.rs`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/storage-values/src/lib.rs`
- `tools/boss/engine/core/src/run_cost.rs`
- `tools/boss/engine/core/src/run_cost/native.rs`
- `tools/boss/engine/core/src/run_cost/native_tests.rs`
- `tools/boss/engine/usage-report/src/record.rs`
- `tools/boss/engine/usage-report/src/rows.rs`
- `tools/boss/engine/usage-report/src/fold.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/engine/usage-report/fixtures/token-usage-seed.sql`
- `tools/boss/protocol/src/types/usage_report.rs`
- `tools/boss/cli/src/cost_cmds.rs`
- `tools/boss/docs/run-token-usage.md`
- `tools/boss/docs/forensic-surfaces.md`

Estimated size: ~19 files / ~1200 changed lines

Effort: large

Dependencies: none

Scope: in-scope

### Review yield report and Deep-criterion breakdown

Scope description: A read-only report over stored review data for a time window (gaps 5 and 6). Per phase (pre-merge round one, round two, round three and later, post-merge) and reviewer role: the number of reviews, findings by severity and by category, verdicts that passed the revision gate, verdicts that passed only because of a low- or medium-severity finding in one of the four always-on categories, the revisions and follow-ups minted, and how many of those changed code and merged. For every batch classified Deep: which criteria fired, and for post-merge reviews the yield per criterion and whether the PR would stay eligible under the size-eligibility treatment. Findings are read from the stored payloads of the review-report and verdict proposals. Criteria are re-derived from each batch's stored classification inputs using the classifier's own thresholds, so nothing new is captured. The command is `boss cost review-yield --since <t> [--until <t>] [--experiment <key>] [--json]`; with `--experiment`, rows split by the arm recorded on each review's execution, which is why this entry follows the mechanism. Not an experiment. It is the guardrail surface for every review experiment and the "pull yield first" step the investigation asks for.

Major deliverables:

1. A pure yield builder with tests, beside the classifier and finding types in the `pr-review` crate, and the snapshot query that feeds it.
2. Request, reply and handler, and `boss cost review-yield` text and JSON output.
3. The Deep-criterion breakdown, taking the thresholds from one definition shared with the classifier and pinned by a test that the two agree on every fixture.

Likely changed files:

- `tools/boss/engine/pr-review/src/parsing.rs`
- `tools/boss/engine/pr-review/src/review_yield.rs` (new)
- `tools/boss/engine/pr-review/src/lib.rs`
- `tools/boss/engine/work-db/src/review_yield_db.rs` (new)
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/storage-values/src/lib.rs`
- `tools/boss/protocol/src/types/review_yield.rs` (new)
- `tools/boss/protocol/src/types.rs`
- `tools/boss/protocol/src/wire.rs`
- `tools/boss/protocol/src/wire/events.rs`
- `tools/boss/protocol/src/request_contract.rs`
- `tools/boss/protocol/src/wire/frontend_event_wire_tests.rs`
- `tools/boss/engine/app/src/app/cost.rs`
- `tools/boss/engine/app/src/app/dispatch.rs`
- `tools/boss/cli/src/commands.rs`
- `tools/boss/cli/src/cost_cmds.rs`

Estimated size: ~20 files / ~1300 changed lines

Effort: large

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Provider quota snapshot history

Scope description: Persist each provider quota reading the engine obtains (driver, window, used percent, reset instant or text, observed time, and failures as failures), sample on a low-rate background cycle in addition to the app-driven probes, and return the history through the existing quota request (gap 1). `boss cost quota --since <t>` prints, per driver and quota window, the change in used percent over each interval beside the engine-computed token totals by class and model for the same interval, so the owner can see which token classes move the quota. Not an experiment: it observes and changes no run. The background cycle sits behind a kill-switch flag and is enabled for a driver only where a probe spends no model tokens, which the entry must confirm per driver.

Major deliverables:

1. A `driver_quota_snapshots` table, a writer on every probe outcome, a retention bound, and the background sampling cycle with its sweep telemetry.
2. History on the existing quota request and reply.
3. `boss cost quota`, showing quota change beside token-class totals for the same intervals.

Likely changed files:

- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/driver_quota_history.rs` (new)
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/app/src/app/engine_meta.rs`
- `tools/boss/engine/app/src/app.rs`
- `tools/boss/engine/driver-quota/src/cache.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/protocol/src/types/driver_quota.rs`
- `tools/boss/protocol/src/wire.rs`
- `tools/boss/cli/src/commands.rs`
- `tools/boss/cli/src/cost_cmds.rs`
- `tools/boss/docs/background-sweep-telemetry.md`

Estimated size: ~15 files / ~900 changed lines

Effort: medium

Dependencies: none

Scope: in-scope

### Refresh Claude list-price weights and price cache writes by lifetime

Scope description: Update the Opus and Sonnet entries in `cost_pricing.rs` from the provider's published price page, add Fable, record the source URL and retrieval date beside each entry, and price 5-minute and 1-hour cache writes at their own rates where a run recorded the split (gap 3). A run with only an unsplit count keeps today's single rate and is labelled. No GPT or Grok entries are added: the usage rows request refuses to price gross-input and subscription-billed drivers whatever the table says. The purpose is that the investigation's "weighted" reading becomes visible per arm through the existing estimated-USD option, and that Fable runs stop being unpriceable. Not an experiment. The cost-attribution design's rate table will later replace this file and take these values and their sources over. This entry does not wait for it.

Major deliverables:

1. Sourced Opus, Sonnet and Fable weights, with tests pinning family matching.
2. Lifetime-split cache-write pricing through the estimator seam, used by `boss cost --usd` and `boss cost rows --usd`.

Likely changed files:

- `tools/boss/engine/core/src/cost_pricing.rs`
- `tools/boss/engine/app/src/app/cost.rs`
- `tools/boss/engine/usage-report/src/classify.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/docs/run-token-usage.md`

Estimated size: ~6 files / ~300 changed lines

Effort: small

Dependencies: none

Scope: in-scope

### Move the file-count protocol and operational-docs list out of the repository agent instructions

Scope description: R3 step 4. `AGENTS.md` at the repository root (`CLAUDE.md` is a symlink to it) is read into every Claude and Codex session in the repository. Move the `change/file-count` split protocol (about 4.6k characters) and the operational-docs list (about 4.0k characters) into two documents read on demand, leaving a short rule and a pointer for each. The hard constraint itself (never bypass a check; split or ask) stays in `AGENTS.md` in full. A straight change and not an experiment: the agent CLI loads this tracked file from the checkout, so two arms cannot see two versions. There is no flag. To read it, first-call context per run in `boss cost rows` falls by about 2k tokens for Claude and Codex runs created after the merge against the runs before it, which is why the context-shape capture has to land first.

Major deliverables:

1. `AGENTS.md` with the two sections reduced to a rule and a pointer each.
2. `docs/change-file-count-protocol.md` and `docs/operational-docs.md` holding the moved text unchanged.

Likely changed files:

- `AGENTS.md`
- `tools/boss/docs/change-file-count-protocol.md` (new)
- `tools/boss/docs/operational-docs.md` (new)

Estimated size: ~3 files / ~250 changed lines

Effort: small

Dependencies: Per-run context shape: first-call context, peak context and compactions in usage rows

Scope: in-scope

### Usage dimensions for experiment arm and pipeline stage

Scope description: Add two dimensions to usage rows. `experiment_arm` reads the launch record's arm set. A new optional `experiment` field on the request names which experiment the dimension refers to, giving the values `treatment`, `control`, `stopped`, `not_enrolled` and `not_recorded`; without it the value is the whole recorded set. The reply lists the experiments observed in the window with run counts and family counts per arm. `pipeline_stage` is an engine classification of each run from its execution kind, its review-batch member role and its task's provenance: filed work, review-finding revision, CI-fix revision, conflict revision, progression-minted work, post-merge follow-up, pre-merge review, review supervisor, post-merge review, review guide, progression, and the remaining kinds by name, with unrecognised input as `unclassified`. `boss cost rows` gains `--experiment` and accepts both dimensions in `--group-by` and `--filter`. If the usage series request has landed by then, it takes the same field and dimensions in this entry. The app's reply decoder is changed in this entry to skip a dimension it does not know (Finding 10), so the landed window keeps working from the moment the engine names the new ones. The picker and columns that use them are the app entry. Not an experiment: this is how every experiment is read.

Major deliverables:

1. Both dimensions in the protocol enum, the projection (arm set, provenance, review role) and the classifier, with tests that each run has exactly one value per dimension, that group sums still equal the window total, and that filters still apply before the run cap.
2. The `experiment` request field, the observed-experiments list in the reply, and a note on any arm comparison computed over a capped reply.
3. `boss cost rows` flags and help text, seed-fixture rows covering control, treatment, stopped, not enrolled, not recorded and each stage, and the app decoder that skips an unknown dimension, with a test that a reply naming one still decodes.

Likely changed files:

- `tools/boss/protocol/src/types/usage_report.rs`
- `tools/boss/engine/usage-report/src/record.rs`
- `tools/boss/engine/usage-report/src/classify.rs`
- `tools/boss/engine/usage-report/src/rows.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/engine/usage-report/fixtures/token-usage-seed.sql`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/app/src/app/cost.rs`
- `tools/boss/cli/src/commands.rs`
- `tools/boss/cli/src/cost_cmds.rs`
- `tools/boss/app-macos/Sources/TokenUsageModels.swift`
- `tools/boss/app-macos/Tests/BossTests/TokenUsageTreeBuilderTests.swift`
- `tools/boss/docs/run-token-usage.md`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~16 files / ~1000 changed lines

Effort: medium

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record; Per-run context shape: first-call context, peak context and compactions in usage rows

Scope: in-scope

### PR-family outcomes report per experiment arm

Scope description: Generalise the Sol comparison into a report for any experiment key (gap 4). A PR family enrols at its first launched execution that records the key. A family in which another execution records a different arm or none, or any execution records `stopped`, is mixed: it is excluded from the comparison and counted. Per arm, and per arm by the driver of the family's first launched execution: families, merged families, tokens per merged family (fully measured families only, with the unmeasured count), first-attempt CI pass rate, revisions per merged PR, review batches per PR, review findings per PR and post-merge follow-ups per merged PR. The family outcome ledger the Sol comparison keeps (baseline head, first head, first CI result, merge time) is opened for every family at its first launch and no longer only for Sol-eligible ones, which also gives a tokens-per-merged-PR figure with no experiment selected. `boss cost experiment` with no key lists every registered experiment and its state: off, split, all, held by its lane, or waiting on a required flag. `boss cost sol` keeps its output. Not an experiment: this is where guardrails are read.

Major deliverables:

1. The family outcome ledger for every family (migration, enrolment at launch, merge and first-CI observation unchanged), with `boss cost sol` output pinned by its existing tests.
2. The outcomes builder and query, with tests for enrolment, mixed-family exclusion, the by-driver split and unmeasured families.
3. Request, reply, handler and `boss cost experiment [<key>]`, including the experiment state listing.

Likely changed files:

- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/driver_allocation.rs`
- `tools/boss/engine/work-db/src/execution_launch_config.rs`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/work-db/src/pr_flow.rs`
- `tools/boss/engine/storage-values/src/lib.rs`
- `tools/boss/engine/core/src/cost_report.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/protocol/src/types/cost_report.rs`
- `tools/boss/protocol/src/wire.rs`
- `tools/boss/protocol/src/wire/events.rs`
- `tools/boss/protocol/src/request_contract.rs`
- `tools/boss/protocol/src/wire/frontend_event_wire_tests.rs`
- `tools/boss/engine/app/src/app/cost.rs`
- `tools/boss/engine/app/src/app/dispatch.rs`
- `tools/boss/cli/src/commands.rs`
- `tools/boss/cli/src/cost_cmds.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~21 files / ~1300 changed lines

Effort: large

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### App: experiment and pipeline-stage grouping in the Token Usage window

Scope description: In the Token Usage window's toolbar, add "Pipeline stage" and "Experiment arm" to the group-by picker and the filter chips, and an experiment picker populated from the reply's observed-experiments list (hidden when the list is empty) that sets the request's `experiment` field. The run inspector shows the recorded arm set, and the per-group call statistics from the context-shape entry appear as columns. The app adds no arithmetic: every figure is a field of the reply. Screenshots against an isolated engine seeded with the fixture show grouping by arm for one experiment, grouping by arm then stage, and the toolbar with no experiments. The window it extends landed in mono#3917.

Major deliverables:

1. The experiment picker and the two group-by options with their chips, all driven by the reply.
2. The arm set in the run inspector and the call-statistics columns, through the window's single figure formatter, with tests.
3. Capture-path screenshots registered with captions.

Likely changed files:

- `tools/boss/app-macos/Sources/TokenUsageWindow.swift`
- `tools/boss/app-macos/Sources/TokenUsageModels.swift`
- `tools/boss/app-macos/Sources/TokenUsageTree.swift`
- `tools/boss/app-macos/Sources/TokenUsageFormatter.swift`
- `tools/boss/app-macos/Sources/TokenUsageCapture.swift`
- `tools/boss/app-macos/Sources/EngineClient+Requests.swift`
- `tools/boss/app-macos/Tests/BossTests/TokenUsageTreeBuilderTests.swift`
- `tools/boss/app-macos/Tests/BossTests/TokenUsageFormatterTests.swift`

Estimated size: ~9 files / ~600 changed lines

Effort: medium

Dependencies: Usage dimensions for experiment arm and pipeline stage

Scope: in-scope

### Experiment: auto-compact window for Claude workers

Scope description: R5 steps 1 and 2 as the experiment `worker_autocompact` (no lane). Treated Claude workers launch with a 200k auto-compact window, and their worker rules carry compact instructions telling the summary to keep the execution context, the task description, the open checklist and the done-declaration command verbatim. Control launches are byte-identical to today. This entry introduces the seam later launch experiments use: a plain launch-options value the runner builds from the arm set and hands to the driver through a defaulted trait method, so the roughly twenty files that build a spawn request by struct literal are not touched and drivers never see an experiment key. Ships inert. To toggle it, use `worker_autocompact` and `worker_autocompact_ab_split` in Debug → Feature Flags. To read it, `boss cost rows --experiment worker_autocompact --group-by experiment_arm --filter driver=claude` shows peak context, runs compacted and tokens per run by arm. The guardrail is the end-status mix of compacted runs and the share that ended without a done declaration.

Major deliverables:

1. The launch-options seam on the driver trait and its use in the Claude launch, with the control launch line pinned unchanged by the existing goldens and a new golden for the treated line.
2. The compact-instructions block in the treated worker rules.
3. Registry entry, flags and the operator-doc row with the read recipe, guardrail and stop condition.

Likely changed files:

- `tools/boss/engine/driver/src/lib.rs`
- `tools/boss/engine/driver/src/claude.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/execution-runner/src/worker_setup.rs`
- `tools/boss/engine/execution-runner/src/host.rs`
- `tools/boss/engine/core/src/worker_setup.rs`
- `tools/boss/engine/core/src/conformance/claude_goldens.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~12 files / ~600 changed lines

Effort: medium

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Runaway-run alarm on a token or round budget

Scope description: R5 step 3. File one attention item per execution when a live run passes 25 million tokens or 150 rounds, read from the cost snapshot the engine already tails, in the manner of the existing wall-clock envelope alarm: a signal only, never a stop. A straight change behind a kill-switch flag `run_budget_alarm` (default on): an alarm changes no run, so there is no arm to compare. The check reads live runs only and must stay within the work database's read-budget rules. To toggle it, use `run_budget_alarm` in Debug → Feature Flags. To read it, the attention item names the run, and the run's row in `boss cost rows` shows the tokens and rounds that tripped it.

Major deliverables:

1. The budget watch over live runs, with the two thresholds as named constants and one item per execution, tested for each threshold, the once-only rule and the flag.
2. The attention kind and its label wherever attention kinds are enumerated, and its entry in the attention lifecycle doc.

Likely changed files:

- `tools/boss/engine/core/src/envelope_watch.rs`
- `tools/boss/engine/core/src/run_budget_watch.rs` (new)
- `tools/boss/engine/core/src/lib.rs`
- `tools/boss/engine/storage-values/src/attention_kinds.rs`
- `tools/boss/engine/app/src/app/server.rs`
- `tools/boss/engine/work-db/src/run_rows.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/docs/attention-lifecycle.md`

Estimated size: ~10 files / ~500 changed lines

Effort: small

Dependencies: none

Scope: in-scope

### Experiment: Claude workers without the personal environment

Scope description: R3 step 1 as the experiment `claude_lean_environment` (no lane). Treated Claude workers of every kind launch with skills and slash commands disabled, no inherited MCP servers, the built-in tools a worker never uses removed by name (the investigation lists `Artifact`, `Workflow`, `ScheduleWakeup`, `SendFeedback` and `ReportFindings`), and the user-level instruction file excluded. The entry first confirms against the pinned CLI version which flags exist and whether restricting the setting sources excludes the user-level file. `--bare` and `--safe-mode` are not used, because they skip the hooks and project instructions Boss relies on. If the user-level file cannot be excluded by a flag, the entry ships the rest and reports a scoped configuration directory as separate work; it does not build one. Ships inert. To toggle it, use `claude_lean_environment` and `claude_lean_environment_ab_split`. To read it, `boss cost rows --experiment claude_lean_environment --group-by experiment_arm,pipeline_stage --filter driver=claude` shows first-call context and first-call cache write per run by arm. The guardrail is that every treated run still delivers hook events, with failed-to-start and blocked rates by arm.

Major deliverables:

1. Lean launch options applied by the Claude driver, with a golden for the treated launch line and a test that the hook settings file is still passed.
2. A check against the pinned CLI version that every flag used is accepted, failing loudly when one is not.
3. Registry entry, flags and the operator-doc row, including the one-time `/context` check the owner runs in a treated session (gap 10).

Likely changed files:

- `tools/boss/engine/driver/src/lib.rs`
- `tools/boss/engine/driver/src/claude.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/core/src/conformance/claude_goldens.rs`
- `tools/boss/engine/core/src/conformance/version_pin.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~9 files / ~500 changed lines

Effort: medium

Dependencies: Experiment: auto-compact window for Claude workers

Scope: in-scope

### Experiment: slim worker rules and brief

Scope description: R3 steps 2 and 3 as the experiment `slim_worker_brief` (lane `worker_prompt`). For treated Standard workers on every driver, each rule is stated once, with the long form in the worker rules and a one-line pointer in the brief. Today the pre-push rebase block, the file-count split protocol, the no-`timeout` rule, the check-bypass rule, the PR body-file recipe and the empty-diff rule are each stated two or three times. Kind-specific text goes only to the kinds that can use it: a run that may touch one markdown file does not receive the file-count split protocol, and the done-declaration section carries only the outcomes and question types its kind can use. Control text is unchanged and stays pinned by the existing prompt tests. Ships inert. To toggle it, use `slim_worker_brief` and `slim_worker_brief_ab_split`. To read it, `boss cost rows --experiment slim_worker_brief --group-by experiment_arm,pipeline_stage` shows first-call context per run by arm for the non-review stages. The guardrail is runs that end with no PR or no done declaration, pushes refused by the check gate, and the blocked rate, by arm.

Major deliverables:

1. Treated variants of the worker rules and the brief sections, selected by one composer parameter, with a test that every rule the control text states appears exactly once in the treated text for each execution kind.
2. Kind gating for the file-count protocol and the done-declaration section, with per-kind tests.
3. Registry entry, flags, the operator-doc row, and a golden for the treated worker rules.

Likely changed files:

- `tools/boss/engine/execution-runner/src/runner/prompt.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/compose_prompt_tests.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/design.rs`
- `tools/boss/engine/execution-runner/src/prompt_fragments.rs`
- `tools/boss/engine/execution-runner/src/worker_setup.rs`
- `tools/boss/engine/execution-runner/src/operator_question.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/execution-runner/src/host.rs`
- `tools/boss/engine/core/src/conformance/claude_goldens.rs`
- `tools/boss/engine/core/src/conformance/goldens/claude_md_standard_slim.md.golden` (new)
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~15 files / ~1300 changed lines

Effort: large

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Experiment: blocking waits instead of polling

Scope description: R4 steps 1 and 3 as the experiment `blocking_waits` (lane `worker_prompt`). Treated Claude Standard workers are told that a command expected to outlast the ten-minute foreground limit is started in the background, declared with the existing wait declaration, and the turn ended, relying on the CLI re-invoking the session when the command exits. The foreground text stays for shorter commands. Treated Codex workers get a longer background-terminal timeout in their per-run configuration and a matching yield in the rules preamble, passed through the launch-options seam. The wake-up on background exit cannot be proved by a test in this repository: the first treated production runs are the end-to-end check, and a treated run that stalls on a wait is the stop condition. Ships inert, and at today's 0% Codex share its Codex half has no traffic. To toggle it, use `blocking_waits` and `blocking_waits_ab_split`. To read it, `boss cost rows --experiment blocking_waits --group-by experiment_arm,driver` shows rounds per run and tokens per run by arm, and poll and wait calls per run once the tool-call-classes entry lands. The guardrail is stale-worker reaps, expired waits and execution duration, by arm.

Major deliverables:

1. Treated Claude brief text for long commands, with tests that the control text is unchanged.
2. The Codex per-run timeout key and preamble yield for treated runs, with configuration-compatibility tests.
3. Registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/execution-runner/src/runner/prompt.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/compose_prompt_tests.rs`
- `tools/boss/engine/execution-runner/src/prompt_fragments.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/driver/src/lib.rs`
- `tools/boss/engine/driver/src/codex.rs`
- `tools/boss/engine/driver/src/codex_tests.rs`
- `tools/boss/engine/driver/src/codex/config_compatibility_tests.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~13 files / ~800 changed lines

Effort: medium

Dependencies: Experiment: slim worker rules and brief; Experiment: auto-compact window for Claude workers

Scope: in-scope

### Experiments: ranged-read and CI-watch tool guards for Claude workers

Scope description: Two experiments on the Claude tool-guard seam, each with its own flags and no lane. `ranged_reads` (R7 step 1): treated Standard workers have a `Read` of a file over 350 lines refused unless it carries an offset and a limit, with a message to locate with a line-numbered search first, and a lower read-output cap as a backstop. Reviewers are never treated, because they must read the diff themselves. `ci_watch_guard` (R4 step 2): treated Standard workers have `gh pr checks --watch` and `gh run watch` refused, with a pointer to the rule that the engine watches CI. The CI-remediation brief instructs `gh pr checks --watch` today, so that kind is exempt and the contradiction is recorded in the operator doc. Unlike the existing safety guards, both fail open on a payload they cannot parse, because a bug in a cost guard must not stop every read. Both are tested by running the script end to end with a JSON payload, as the existing guards are. Ships inert. To toggle them, use `ranged_reads`, `ci_watch_guard` and their `_ab_split` flags. To read them, `boss cost rows --experiment ranged_reads --group-by experiment_arm --filter driver=claude` shows tokens per run and rounds per run by arm, and read-result size per run once the tool-call-classes entry lands; the same with `ci_watch_guard` shows rounds per run. The guardrail is that rounds per run do not rise for `ranged_reads`, and runs ending blocked after a refusal for either.

Major deliverables:

1. The `ranged_reads` guard and read-output cap, wired only for treated Standard workers, with end-to-end script tests.
2. The `ci_watch_guard` guard with the CI-remediation exemption, with end-to-end script tests.
3. Registry entries, flags, operator-doc rows, and the hook-composition test for treated runs.

Likely changed files:

- `tools/boss/engine/driver/src/lib.rs`
- `tools/boss/engine/driver/src/claude.rs`
- `tools/boss/engine/driver/src/read_range_guard.py` (new)
- `tools/boss/engine/driver/src/ci_watch_guard.py` (new)
- `tools/boss/engine/core/src/worker_setup.rs`
- `tools/boss/engine/core/src/worker_setup_tests/read_range_guard.rs` (new)
- `tools/boss/engine/core/src/worker_setup_tests/ci_watch_guard.rs` (new)
- `tools/boss/engine/core/src/worker_setup_tests/pre_tool_use_composition.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~14 files / ~900 changed lines

Effort: medium

Dependencies: Experiment: Claude workers without the personal environment

Scope: in-scope

### Experiment: slim post-merge review

Scope description: R1 steps 1 and 2 as the experiment `post_merge_review_slim` (lane `post_merge_review`, requires `review_batch_fanout`). For a treated family, the post-merge reviewer runs at `medium` effort, the pre-merge reviewers' level, instead of `large`; its brief embeds the merged diff under the existing line cap with lockfiles excluded; and a PR whose work item was itself minted by a post-merge review is not post-merge reviewed. Pre-merge briefs have no lockfile exclusion today, so the exclusion is added to the shared embed helper as an option used only here. The merge poller receives the arm through its host interface. The comment that says `large` means Claude `high` is corrected. Ships inert. To toggle it, use `post_merge_review_slim` and `post_merge_review_slim_ab_split`. To read it, `boss cost rows --experiment post_merge_review_slim --group-by experiment_arm --filter pipeline_stage=post_merge_review` shows runs and tokens per run by arm. The guardrail is critical and high findings per post-merge review and the share of follow-ups that merge, by arm, from `boss cost review-yield --experiment post_merge_review_slim`.

Major deliverables:

1. Effort and embedded diff for the treated post-merge reviewer, with the lockfile exclusion and the line cap tested on the embed helper.
2. The recursion guard for families minted by a post-merge review, tested through the merge poller's trigger in both arms.
3. Registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/merge-poller/src/post_merge_review.rs`
- `tools/boss/engine/merge-poller/src/host.rs`
- `tools/boss/engine/merge-poller/src/sweep.rs`
- `tools/boss/engine/core/src/merge_poller_adapters.rs`
- `tools/boss/engine/work-db/src/review_batches.rs`
- `tools/boss/engine/pr-review/src/post_merge_render.rs`
- `tools/boss/engine/pr-review/src/render.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~14 files / ~800 changed lines

Effort: medium

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Experiments: post-merge size eligibility and follow-up severity bar

Scope description: Two experiments in lane `post_merge_review`, each with its own flags. `post_merge_size_eligibility` (R1 step 3): for a treated family a merged PR is post-merge reviewed when it changed more than 1,000 lines or 25 files, or raised two or more complexity flags and changed at least 400 lines. Path words alone no longer qualify. The Deep classifier that picks the pre-merge reviewer's model is not changed. `post_merge_followup_bar` (R1 step 4): for a treated family a post-merge finding mints a follow-up task only at critical or high severity, and lower findings stay recorded on the verdict. The investigation offered eligibility by size alone or by flags plus a floor. This tests the second, and the 400-line floor is this design's choice, to be revisited when the yield report shows which criteria produce merged fixes. Ships inert. To toggle them, use each key and its `_ab_split`. To read them, `boss cost rows --experiment <key> --group-by experiment_arm,pipeline_stage` shows post-merge review runs and post-merge follow-up runs and tokens by arm, and `boss cost experiment <key>` shows post-merge follow-ups per merged PR. The guardrail comes from `boss cost review-yield` in the control arm: the merged critical and high follow-ups that came from PRs the treatment would have excluded, or from findings it would have suppressed.

Major deliverables:

1. The eligibility predicate, shared with the yield report, and its use in the post-merge trigger for treated families.
2. A phase-aware gate for post-merge verdicts in treated families, leaving the pre-merge gate untouched.
3. Registry entries, flags and operator-doc rows.

Likely changed files:

- `tools/boss/engine/pr-review/src/parsing.rs`
- `tools/boss/engine/merge-poller/src/post_merge_review.rs`
- `tools/boss/engine/merge-poller/src/host.rs`
- `tools/boss/engine/core/src/merge_poller_adapters.rs`
- `tools/boss/engine/work-db/src/review_verdict_apply.rs`
- `tools/boss/engine/core/src/review_verdict_apply_sweep.rs`
- `tools/boss/engine/app/src/app/proposals.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~12 files / ~700 changed lines

Effort: medium

Dependencies: Experiment: slim post-merge review; Review yield report and Deep-criterion breakdown

Scope: in-scope

### Experiment: delta re-review from the second round

Scope description: R2 step 1 as the experiment `delta_rereview` (lane `pre_merge_review`, requires `review_batch_fanout`). For a treated family, review rounds after the first use one reviewer at the Light tier with no supervisor. Its brief carries the prior round's findings, the diff since the last reviewed commit and the changed-file list. It asks the reviewer to mark each prior finding resolved or not, to review only the delta, and to run one explicit whole-PR duplication check over the changed-file list, which is the check the current "do not scope your review to a diff-only view" wording exists for. The preferred shape is to route these rounds through the existing single-reviewer path, the one that runs when fan-out is off, with the delta brief and the Light-tier model, so that no new batch shape or quorum rule is needed. The entry must confirm that cycle counting, the stale-head and duplicate-head guards and post-merge eligibility read the same state on both paths. If they do not, it adds a one-member batch and says so. The trigger predicate is repeated at three sites today and all three must agree. Ships inert. To toggle it, use `delta_rereview` and `delta_rereview_ab_split`. To read it, `boss cost rows --experiment delta_rereview --group-by experiment_arm,pipeline_stage` shows pre-merge review and supervisor runs and tokens by arm, and `boss cost experiment delta_rereview` shows review batches per PR, revisions per merged PR and tokens per merged family. The guardrail is post-merge critical and high findings per merged PR, and duplication findings at post-merge, by arm.

Major deliverables:

1. Routing of the second and later rounds for treated families, with tests on all three trigger sites and on cycle counting across the two paths.
2. The delta brief with prior findings, delta diff, changed-file list and the explicit duplication step, with rendering tests.
3. Registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/completion/src/pr_transition.rs`
- `tools/boss/engine/completion/src/run_done_declaration.rs`
- `tools/boss/engine/completion/src/finalize_passes.rs`
- `tools/boss/engine/completion/src/tests/delta_rereview.rs` (new)
- `tools/boss/engine/pr-review/src/render.rs`
- `tools/boss/engine/pr-review/src/parsing.rs`
- `tools/boss/engine/work-db/src/review_batches.rs`
- `tools/boss/engine/work-db/src/pr_flow.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~16 files / ~1200 changed lines

Effort: large

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Experiments: revision severity bar and minimum changed lines for re-review

Scope description: Two experiments in lane `pre_merge_review`, each with its own flags. `revision_gate_bar` (R2 step 3): for a treated family, a finding in one of the four always-on categories (regression, duplication, deferred scope, agent-isms) mints a revision only at medium severity or above. Critical and high findings still always do. Findings below the bar are posted to the PR in one comment and not dropped. `min_review_changed_lines` (R2 step 2): for a treated family, a revision push that changes fewer than 10 lines since the last reviewed commit does not trigger a re-review. A PR's first review is never skipped. The investigation says only "above 0"; 10 is this design's choice, the smallest bound that covers one-line CI and conflict fixes. The startup setting `BOSS_MIN_REVIEW_CHANGED_LINES` keeps working for the control arm and for installations that set it. Ships inert. To toggle them, use each key and its `_ab_split`. To read them, `boss cost rows --experiment <key> --group-by experiment_arm,pipeline_stage` shows review-finding revision runs for `revision_gate_bar` and pre-merge review runs for `min_review_changed_lines`, with their tokens, by arm, and `boss cost experiment <key>` shows revisions per merged PR and review batches per PR. The guardrail is post-merge findings per merged PR in the four categories for `revision_gate_bar`, and first-attempt CI pass rate with post-merge findings for `min_review_changed_lines`.

Major deliverables:

1. The severity bar for the four categories in treated families, on both gate callers, with the comment that carries the findings below the bar.
2. The changed-lines skip for treated families through the existing no-op skip gate, recorded with its existing skip outcome.
3. Registry entries, flags and operator-doc rows.

Likely changed files:

- `tools/boss/engine/pr-review/src/parsing.rs`
- `tools/boss/engine/pr-review/src/render.rs`
- `tools/boss/engine/work-db/src/review_verdict_apply.rs`
- `tools/boss/engine/core/src/review_verdict_apply_sweep.rs`
- `tools/boss/engine/completion/src/finalize_passes.rs`
- `tools/boss/engine/completion/src/pr_transition.rs`
- `tools/boss/engine/completion/src/handler_build.rs`
- `tools/boss/engine/completion/src/tests/revision_gate_bar.rs` (new)
- `tools/boss/engine/app/src/app/proposals.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~14 files / ~800 changed lines

Effort: medium

Dependencies: Experiment: delta re-review from the second round

Scope: in-scope

### Experiment: review guide once per PR when it is ready

Scope description: R6 as the experiment `review_guide_when_ready` (no lane, requires `review_guide_generation`). For a treated family, a new source capture no longer enqueues a guide. A guide is generated when the PR is ready for a person (no live review batch or revision for the family, and the head's required checks green, as the merge poller observes them) or when someone opens the PR's guide in the app and none exists for the current comparison, whichever comes first, and it runs at `small` effort (Codex `medium`) instead of today's pinned `medium` (Codex `high`). The app asks for a guide on open in both arms through the existing generate request, which the engine already limits to one live attempt per PR, so the app needs no knowledge of arms. The engine counts guide opens and opens that found no current guide. The investigation named both triggers; this tests them together and cannot say which one mattered. Ships inert. To toggle it, use `review_guide_when_ready` and `review_guide_when_ready_ab_split`. To read it, `boss cost rows --experiment review_guide_when_ready --group-by experiment_arm --filter pipeline_stage=review_guide` shows guide runs and tokens by arm. The guardrail is the count of opens that found no current guide and the wait from open to guide, in the Metrics window.

Major deliverables:

1. The ready-or-opened trigger and `small` effort for treated families, with tests on the capture, poller and manual paths.
2. Generate-on-open in the app, and the two open counters in the engine.
3. Registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/completion/src/review_guide_capture.rs`
- `tools/boss/engine/merge-poller/src/sweep.rs`
- `tools/boss/engine/merge-poller/src/host.rs`
- `tools/boss/engine/core/src/merge_poller_adapters.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/app/src/app/review_guide.rs`
- `tools/boss/engine/app/src/app/tests/review_guide.rs`
- `tools/boss/app-macos/Sources/ChatViewModel+ReviewGuide.swift`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~13 files / ~800 changed lines

Effort: medium

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Experiments: model and effort routing

Scope description: R9 as three experiments on disjoint populations, each with its own flags and no lane. `supervisor_standard_tier`: for a treated family the review supervisor runs on the Balanced tier (Sonnet) whatever the batch's profile. `ci_fix_standard_tier`: for a treated family a CI-fix revision minted with no failure evidence gets standard reasoning at `medium` effort and is no longer pinned to investigation reasoning. The investigation's alternative, fetching the failing log first, is not tested: an empty evidence list is what is left after the watcher already tried. `investigation_routing`: for a treated family on the Claude driver, a design- or investigation-tier run at `large` effort launches at `high` instead of `xhigh`, and one at `small` or `medium` effort launches on the investigation model (Opus) instead of the design model (Fable). First-round Deep reviewers stay on Opus. Ships inert. To toggle them, use each key and its `_ab_split`. To read them, `boss cost rows --experiment <key> --group-by experiment_arm,model` with `--filter pipeline_stage=review_supervisor`, `--filter pipeline_stage=ci_fix_revision`, or `--filter task_kind=investigation,design` shows runs, tokens and output tokens per run by arm and model. The guardrails are consolidated findings per batch against leaf findings for the supervisor, repeat CI-fix revisions per PR for the CI-fix tier, and blocked and failed rates with document revisions per run for investigation routing.

Major deliverables:

1. The supervisor tier for treated families, passed into supervisor creation, with tests.
2. CI-fix reasoning for treated families through the merge poller's host interface, with tests.
3. Investigation routing for treated families in spawn resolution, with tests per effort level, and the registry entries, flags and operator-doc rows for all three.

Likely changed files:

- `tools/boss/engine/work-db/src/review_batches.rs`
- `tools/boss/engine/completion/src/finalize_passes.rs`
- `tools/boss/engine/merge-poller/src/ci_watch.rs`
- `tools/boss/engine/merge-poller/src/host.rs`
- `tools/boss/engine/core/src/merge_poller_adapters.rs`
- `tools/boss/engine/effort/src/lib.rs`
- `tools/boss/engine/driver/src/claude.rs`
- `tools/boss/engine/execution-runner/src/runner/worker_spawn.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~14 files / ~900 changed lines

Effort: medium

Dependencies: Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Per-run tool-call classes: reads, searches, polls and waits

Scope description: Gaps 8 and 9 and R4 step 4. From the same transcript pass that captures usage, count each run's tool calls by class (file read, search, edit, build or test, poll, wait, CI watch, other), with the result size of each class when first written, and for polls the requested yield and the wall-clock gap, summed per run. Store them in one nullable column on the run, and surface them per run and as engine-computed per-run averages on every group in usage rows. Classes for shell commands are assigned by a small, tested pattern table and are labelled approximate. Not an experiment. To read it, `boss cost rows --group-by pipeline_stage,driver` shows calls per run by class, and with `--experiment` it is the mechanism check for the waits, guards and search experiments.

Major deliverables:

1. The classification table and per-run capture for Claude transcripts and Codex rollouts, NULL for a driver whose transcript carries no tool calls, with a test per class.
2. Storage and usage rows fields, with per-group averages computed by the engine.
3. `boss cost rows` text columns and documentation.

Likely changed files:

- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/run_rows.rs`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/storage-values/src/lib.rs`
- `tools/boss/engine/core/src/run_cost.rs`
- `tools/boss/engine/core/src/run_cost/tool_classes.rs` (new)
- `tools/boss/engine/usage-report/src/record.rs`
- `tools/boss/engine/usage-report/src/rows.rs`
- `tools/boss/engine/usage-report/src/fold.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/engine/usage-report/fixtures/token-usage-seed.sql`
- `tools/boss/protocol/src/types/usage_report.rs`
- `tools/boss/cli/src/cost_cmds.rs`
- `tools/boss/docs/run-token-usage.md`

Estimated size: ~17 files / ~1200 changed lines

Effort: large

Dependencies: Usage dimensions for experiment arm and pipeline stage

Scope: in-scope

### Usage end-reason breakdown and overlapping-execution caveat

Scope description: R10 and gap 11. Add an `end_reason` usage dimension: each run's execution end status, refined by a stable reason code derived from the done-declaration outcome and from the error text the engine itself writes when it fails, orphans, cancels or abandons an execution. Each engine writer gets a code, and text that matches none is `other`. Add a run caveat for an implementation execution whose active interval overlaps another live implementation execution on the same work item, and count the double-spawn guard's fail-open path so that an overlap can be matched to it. Nothing is fixed here: the investigation's instruction is to break the waste down by reason before fixing anything, and fixes are filed from what this shows. Not an experiment, because a defect has no control arm worth keeping. To read it, `boss cost rows --group-by end_reason` shows tokens by reason, the Token Usage window offers the same grouping through one new case in the app's dimension list, and the overlap caveat marks the runs to trace.

Major deliverables:

1. Reason codes for the engine's own end-of-execution writers and the `end_reason` dimension, with a test per code.
2. The overlapping-execution caveat computed by the builder, and a counter on the guard's fail-open path.
3. `boss cost rows` help text, fixture rows and documentation.

Likely changed files:

- `tools/boss/protocol/src/types/usage_report.rs`
- `tools/boss/engine/usage-report/src/record.rs`
- `tools/boss/engine/usage-report/src/classify.rs`
- `tools/boss/engine/usage-report/src/rows.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/engine/usage-report/fixtures/token-usage-seed.sql`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/work-db/src/executions_runs.rs`
- `tools/boss/engine/dispatch/src/coordinator/execution.rs`
- `tools/boss/cli/src/commands.rs`
- `tools/boss/app-macos/Sources/TokenUsageModels.swift`
- `tools/boss/docs/run-token-usage.md`

Estimated size: ~14 files / ~800 changed lines

Effort: medium

Dependencies: Per-run tool-call classes: reads, searches, polls and waits

Scope: in-scope

### Coordinator session usage in usage rows

Scope description: The coordinator session is not an execution, so its tokens, about 4% of the measured week, appear nowhere the Token Usage window reads (Finding 6). Tail the coordinator's transcript on a low-rate cycle with the shared usage accumulator and record, per session and day: token classes, rounds, peak context, compactions, the number of turns that began with an inbound cross-session message (R8 step 3), and the coordinator experiment flags in force. Emit those rows in the usage projection as taskless records of kind `coordinator_session` (pipeline stage `coordinator_session`, spend class overhead), so they count in the window total and group like any other run. The coordinator's subagents stay excluded and carry the existing subagents-excluded caveat. Not an experiment. It is what makes the coordinator experiments readable and what gives size-based rotation its trigger. To read it, `boss cost rows --filter pipeline_stage=coordinator_session --group-by model` shows tokens, rounds and peak context per day.

Major deliverables:

1. The coordinator transcript tail and per-session-day storage, with sweep telemetry and tests over a recorded transcript fixture that includes a compaction and inbound turns.
2. Taskless `coordinator_session` records in the usage projection and classifier, with the window total still equal to the sum of its parts.
3. Documentation: the usage docs, the sweep telemetry doc and the coordinator handoff doc.

Likely changed files:

- `tools/boss/engine/work-db/src/schema_migration_registry.rs`
- `tools/boss/engine/work-db/src/schema_init.rs`
- `tools/boss/engine/work-db/src/coordinator_usage.rs` (new)
- `tools/boss/engine/work-db/src/lib.rs`
- `tools/boss/engine/work-db/src/cost_report_db.rs`
- `tools/boss/engine/core/src/coordinator_usage_tail.rs` (new)
- `tools/boss/engine/core/src/coordinator_tmux.rs`
- `tools/boss/engine/core/src/lib.rs`
- `tools/boss/engine/usage-report/src/classify.rs`
- `tools/boss/engine/usage-report/src/tests.rs`
- `tools/boss/engine/usage-report/fixtures/token-usage-seed.sql`
- `tools/boss/docs/run-token-usage.md`
- `tools/boss/docs/background-sweep-telemetry.md`
- `tools/boss/docs/coordinator-session-handoff.md`

Estimated size: ~16 files / ~1100 changed lines

Effort: large

Dependencies: Usage end-reason breakdown and overlapping-execution caveat

Scope: in-scope

### Experiments: coordinator auto-compact window and size-based rotation

Scope description: R8 steps 1 and 2 as two all-or-nothing experiments in lane `coordinator_context`. There is one coordinator at a time, so there is no concurrent control and no split flag. `coordinator_autocompact`: the coordinator launches with a 150k auto-compact window. `coordinator_size_rotation`: when the observed context passes 150k, the engine rotates to a fresh session through the existing handoff, with a new recreate reason. They are alternatives for the same budget, so this is a choice between them: the lane lets only one run, and the comparison is between weeks of compaction, rotation and neither. A flag change takes effect at the next coordinator session, and a manual coordinator reset applies it at once. Ships inert. To toggle them, use `coordinator_autocompact` or `coordinator_size_rotation` in Debug → Feature Flags. To read them, `boss cost rows --filter pipeline_stage=coordinator_session --group-by experiment_arm` shows tokens per day, tokens per round and compactions with the arm recorded on each session-day. The guardrail is a manual read of the handoff after a rotation or compaction, coordinator resets per week, and rotations per day, where more than a few a day means a loop.

Major deliverables:

1. The auto-compact window on the coordinator launch when its flag is on, with the launch-line test.
2. Size-triggered rotation through the handoff path, with a minimum interval between rotations, a new recreate reason, and tests with a fake context reading.
3. Registry entries, flags, operator-doc rows, and the handoff doc's list of triggers.

Likely changed files:

- `tools/boss/engine/core/src/coordinator_tmux.rs`
- `tools/boss/engine/core/src/coordinator_tmux/inbox.rs`
- `tools/boss/engine/core/src/coordinator_handoff.rs`
- `tools/boss/engine/core/src/coordinator_usage_tail.rs`
- `tools/boss/engine/app/src/app/sessions.rs`
- `tools/boss/protocol/src/wire/coordinator_recreate.rs`
- `tools/boss/app-macos/Sources/EngineEvent.swift`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/coordinator-session-handoff.md`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~13 files / ~800 changed lines

Effort: medium

Dependencies: Coordinator session usage in usage rows; Experiment registry, PR-family assignment and arm recording in the launch record

Scope: in-scope

### Experiment: exploration subagent on a cheaper model

Scope description: R7 step 3 as an experiment in lane `worker_prompt`. Define a worker subagent on a cheap model in the per-run agent directory, and tell treated Claude implementation workers to send "where does X live" questions to it and to re-read before editing. Reviewers are never treated. It is deferred because Claude subagent tokens are not captured (Finding 7): the treated arm's subagent usage would be invisible and the treatment would look cheaper than it is. It needs the cost-attribution design's subagent transcript enumeration first.

Major deliverables:

1. The agent definition written per run, and the routing text for treated workers, with tests.
2. Registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/execution-runner/src/runner/prompt.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/compose_prompt_tests.rs`
- `tools/boss/engine/core/src/worker_setup.rs`
- `tools/boss/engine/execution-runner/src/runner/pane_spawn.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~8 files / ~500 changed lines

Effort: medium

Dependencies: Experiment: blocking waits instead of polling

Scope: deferred (future / not a v1 blocker) — unreadable until Claude subagent tokens are captured, which the cost-attribution design owns.

### Experiment: on-demand crate map

Scope description: R7 step 4 as an experiment in lane `worker_prompt`: a crate map kept out of the fixed context and read on demand, with one pointer line in the treated brief. It is deferred because the investigation leaves open how the map is produced and kept current, and because it targets the same locating calls as the exploration subagent, so its value depends on that result.

Major deliverables:

1. The map and the way it is kept current, with a freshness check.
2. The pointer line for treated workers, registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/docs/crate-map.md` (new)
- `tools/boss/engine/execution-runner/src/runner/prompt.rs`
- `tools/boss/engine/execution-runner/src/runner/prompt/compose_prompt_tests.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~7 files / ~500 changed lines

Effort: small

Dependencies: Experiment: exploration subagent on a cheaper model

Scope: deferred (future / not a v1 blocker) — the investigation leaves the map's upkeep open, and it competes with the exploration subagent for the same calls.

### Experiment: review guide as a single utility-model call

Scope description: R6's longer-term step as an experiment: for a treated family the guide is produced by one direct model call through the utility-model path and not by an agent session. It is deferred because the investigation places it after the once-per-PR change, because a single call cannot explore the repository, and because whether it is worth building depends on how many guides remain and whether they are opened once `review_guide_when_ready` has been read.

Major deliverables:

1. A guide task on the utility-model path fed by the captured source packet, with tests.
2. Selection between the agent session and the single call for treated families, registry entry, flags and the operator-doc row.

Likely changed files:

- `tools/boss/engine/utility-model/src/task.rs`
- `tools/boss/engine/utility-model/src/lib.rs`
- `tools/boss/engine/review-guide/src/lib.rs`
- `tools/boss/engine/completion/src/review_guide_capture.rs`
- `tools/boss/engine/app/src/app/review_guide.rs`
- `tools/boss/engine/feature-flags/src/lib.rs`
- `tools/boss/engine/feature-flags/src/experiments.rs`
- `tools/boss/docs/token-experiments.md`

Estimated size: ~10 files / ~900 changed lines

Effort: medium

Dependencies: Experiment: review guide once per PR when it is ready

Scope: deferred (future / not a v1 blocker) — the investigation's longer-term step, to be decided after the once-per-PR experiment is read.
