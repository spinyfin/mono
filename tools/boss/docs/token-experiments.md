# Token experiments: starting, splitting, stopping and reading

A token experiment is a change meant to cut Boss's token spend, shipped behind flags so half of the work gets it and half does not, and so it can be stopped in one click. This page is the operator reference: how to run one, what a launch records about it, and what each experiment changes. The reasoning behind the mechanism, and the list of experiments still to come, is in [the design](designs/token-consumption-reduction-experiments.md).

Every experiment ships **inert**: its flag is off, and a build that contains it behaves exactly as before until you switch it on.

## What is on and what is not yet

The mechanism and its first experiment are in the engine. The things that make an experiment readable are not:

- **Landed:** the experiment registry and flags, the arm assigned to every launch, the `experiment_arms` column in the launch record, the `pane_spawned` dispatch event carrying the arm set, and usage rows and series that accept `--experiment <key>` with the `experiment_arm` and `pipeline_stage` dimensions (replies list observed experiments with run and family counts per arm).
- **Not landed:** the per-family outcomes report and the Token Usage window picker.

**Do not switch an experiment on until the family outcomes report has landed.** The CLI can now compare recorded token usage by arm, but evaluating savings also needs the family outcomes and guardrails. The Token Usage window picker is still pending. Recorded arms are durable, so work launched in the meantime is readable afterwards; the experiment's treatment is live for its treated half the whole time.

## Starting, splitting and stopping

All switches are in the macOS app under **Debug → Feature Flags**, section `experiments`. There is no new request, command or setting.

| Flag                | Default | Effect                                                                                                |
| ------------------- | ------- | ----------------------------------------------------------------------------------------------------- |
| `<key>`             | off     | The experiment's own switch. Off: every family gets today's behaviour and nothing is recorded for it. |
| `<key>_ab_split`    | on      | On: half of PR families are treated. Off: every family is treated.                                    |
| `token_experiments` | on      | Master switch. Off: every experiment resolves to off at once.                                         |

So there are three states per experiment:

- **Off** (the shipped state): `<key>` off, or `token_experiments` off.
- **Split:** `<key>` on and `<key>_ab_split` on. Half of PR families are treated and half are control.
- **All:** `<key>` on and `<key>_ab_split` off. Every family is treated and there is no control. Use it to adopt a treatment, not to measure one.

**To stop an experiment,** turn `<key>` off, or turn `token_experiments` off to stop all of them. The next launch and the next policy decision get today's behaviour. A worker that is already running keeps the brief and launch options it started with, because they cannot be withdrawn. If the fault is in the launch itself, cancel the treated runs still in flight.

The flags are read live on every launch. Nothing is cached and nothing needs an engine restart.

### Flipping flags mixes families in flight

A PR family is assigned an arm at each of its executions' first launch, from the flags as they are then. If you flip a flag while a family is in flight, its later executions can launch under a different arm, or under none, and the family is mixed. A retry of one execution is different: it reuses the arm its first launch recorded, or is marked `stopped` if the experiment was turned off in between (see below). The family outcomes report (not landed yet) is designed to leave mixed families, and any family with a `stopped` execution, out of its comparison and count them. **Flipping a split on and off repeatedly throws away most of a week.** Turn an experiment on once, leave it, and turn it off once.

## Who is in which arm

- **The unit is the PR family:** the chain root work item and everything under it (its revisions, reviews, CI and conflict fixes, and review guide). They share one arm, so per-PR measures such as revisions per PR and tokens per merged PR mean something.
- **The draw is a stable hash.** A family's bucket in `[0, 100)` is a SHA-256 of `experiment:<key>:<family root id>`. Below 50 is treatment, 50 and above is control. Including the key makes two experiments' draws independent of each other. The same family gets the same arm at every launch for as long as the flags stay put.
- **Remote workers are never enrolled.** The remote spawn path reads no feature flags, so a run on a remote host records an empty arm set and is never treated. If remote hosts take a meaningful share of work, a half-and-half split is no longer half of everything.
- **Only Standard workers read the batched-search rule. Only Claude workers get the auto-compact window.** Every execution records the family's arm for every running experiment, whether or not that run's own launch differs; the per-experiment table below names the filters that isolate the runs an experiment changes.

## What a launch records

The launch record is the `work_executions` row, written once per execution at its first launch by one writer, in the same transaction as the driver, model and effort level. The arm set is `work_executions.experiment_arms`: a JSON object from experiment key to `control`, `treatment` or `stopped`, keys sorted, no whitespace.

| Stored value                     | Meaning                                                                                         |
| -------------------------------- | ----------------------------------------------------------------------------------------------- |
| `{"batched_search":"treatment"}` | The experiment was running at this launch and the family was treated.                           |
| `{"batched_search":"control"}`   | Running, and the family was control. Control is recorded as explicitly as treatment.            |
| `{"batched_search":"stopped"}`   | Running at the first launch, turned off before a later run of the same execution.               |
| `{}`                             | Recorded, and no experiment was running.                                                        |
| NULL                             | Not recorded: the execution launched before the column existed, or its launch was not recorded. |

A missing key never means control. It means the experiment was not running for that launch, or nothing was recorded.

- **Flags may change at any time.** What was in force for a run is recovered from the run's own row. Analysis never depends on when a flag was flipped.
- **Sticky.** A later run of the same execution (a retry) reuses the stored set, so the record stays true. An experiment that started after the first launch is not picked up by a later run, and an execution that recorded NULL is never treated.
- **A stop rewrites the record.** With `<key>` off, a later run of an execution that recorded the key launches untreated, and the writer changes that key's value to `stopped`, whichever arm it held. Control is rewritten as well as treatment so both arms lose such executions alike. This is the only update made after the first launch. The tokens a treated first run used are then counted in neither arm.
- **A treatment's definition is fixed for the life of its key.** Changing a threshold or the wording is a new key, so old and new data are never pooled under one name. A key is never reused once its experiment has been adopted or dropped.

The same arm set rides the `pane_spawned` dispatch event, under `details.spawn_config.experiment_arms`, as an object (null when the runner that launched the run does not run experiments). The arm set is deliberately not part of the protocol's `WorkExecution` type.

## Lanes: what may not run together

Two experiments that rewrite the same instructions, or reshape the same review, cannot be told apart by any amount of careful assignment, because the treated run received both. A lane is a named surface on which only one experiment may be assigned at a time.

| Lane                  | Surface                                                         |
| --------------------- | --------------------------------------------------------------- |
| `worker_prompt`       | The rules and brief text a Standard worker is given.            |
| `pre_merge_review`    | Which pre-merge reviews run, their shape, and what they mint.   |
| `post_merge_review`   | Whether a post-merge review runs, its shape, and what it mints. |
| `coordinator_context` | How the coordinator's context is bounded.                       |

**The rule is enforced, not advised.** If two experiments in one lane are switched on, the one registered first runs and the other resolves to off and records nothing. An experiment held back by a missing required flag does not hold its lane. Experiments with no lane may overlap with anything and are kept apart by independent assignment.

Run at most four experiments at once. The fall-back read for a surprising result is one experiment inside the control arms of the others, and each extra experiment halves the families per arm.

## Experiments

### `batched_search`

| Property              | Value                                                                                                                                                                                                                                                                                                                      |
| --------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Source                | Step 2 of recommendation R7 in [the token-consumption investigation](investigations/token-consumption-reduction-2026-11-19.md).                                                                                                                                                                                            |
| Lane                  | `worker_prompt`                                                                                                                                                                                                                                                                                                            |
| Requires              | Nothing beyond `token_experiments`.                                                                                                                                                                                                                                                                                        |
| Runs it changes       | Standard workers on every driver: task, chore, investigation, revision, conflict-resolution, CI-remediation and design runs.                                                                                                                                                                                               |
| Treatment             | The brief gains one rule, directly under its opening line: issue independent searches, listings and file reads together as parallel tool calls in a single turn, and sequence calls only when a later one needs an earlier result. Reviewers, triage, answer agents, progression runs and review guides are never treated. |
| Control               | Today's brief, byte for byte.                                                                                                                                                                                                                                                                                              |
| Estimated saving      | Part of the 0.3–0.6B tokens a week the investigation labels experimental.                                                                                                                                                                                                                                                  |
| Risk                  | Low. A rule in the brief; no tool is withdrawn.                                                                                                                                                                                                                                                                            |
| What shows the effect | Rounds per run and tokens per run on non-review stages, by arm. Filter to Standard worker executions: the arm is recorded for every execution in the family, treated or not.                                                                                                                                               |
| Guardrail             | Review findings per PR, from the outcomes report.                                                                                                                                                                                                                                                                          |
| Stop early when       | A treated run cannot start, or ends in a state the engine cannot classify. No other condition beyond the general rules below.                                                                                                                                                                                              |

To run it: switch `batched_search` on (a half-and-half split starts), switch `batched_search_ab_split` off to treat every family, switch `batched_search` off to stop. To read it, `boss cost rows --experiment batched_search --group-by experiment_arm`.

### `worker_autocompact`

| Property              | Value                                                                                                                                                                                                                                                                 |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Source                | Steps 1 and 2 of recommendation R5 in [the token-consumption investigation](investigations/token-consumption-reduction-2026-11-19.md).                                                                                                                                |
| Lane                  | None. It may overlap any other experiment.                                                                                                                                                                                                                            |
| Requires              | Nothing beyond `token_experiments`.                                                                                                                                                                                                                                   |
| Runs it changes       | Claude workers of every kind. Other drivers in a treated family record the arm but launch as they do today.                                                                                                                                                           |
| Treatment             | The launch line gains `--autocompact 200k`, and the worker rules gain a compact-instructions block (`## Compact Instructions`) telling the summary to keep the execution context, the task description, the open checklist and the done-declaration command verbatim. |
| Control               | Today's launch line and worker rules, byte for byte.                                                                                                                                                                                                                  |
| Estimated saving      | ≈0.4–0.5B tokens a week.                                                                                                                                                                                                                                              |
| Risk                  | Low to medium. A summary can drop a constraint; the compact-instructions block and the existing nudges cover the protocol parts.                                                                                                                                      |
| What shows the effect | Peak context, runs compacted and tokens per run, by arm, filtered to Claude. The arm is recorded for every execution in the family, treated or not.                                                                                                                   |
| Guardrail             | End-status mix of compacted runs, and the share that ended without a done declaration.                                                                                                                                                                                |
| Stop early when       | A compacted run loses its task or its done declaration, or the undeclared rate is twice control. Also the general rule below.                                                                                                                                         |

To run it: switch `worker_autocompact` on (a half-and-half split starts), switch `worker_autocompact_ab_split` off to treat every family, switch `worker_autocompact` off to stop. To read it, `boss cost rows --experiment worker_autocompact --group-by experiment_arm --filter driver=claude`.

## Rules for every experiment

- **A hard failure stops an experiment at once.** A treated run that cannot start, loses its hooks, or ends in a state the engine cannot classify is a reason to turn the key off without waiting for a count.
- **A rate guardrail is read once each arm has 40 units** (families, or affected runs for a run-grain rate). Before that only the hard-failure rule applies.
- **The stop conditions act during the experiment,** through the kill switch. They are not a gate at the end of it.
- **Launch behaviour is proved by real runs.** A golden test pins a string. Whether a driver honours a flag or a rule only shows in the first treated runs.
- **Adopt or drop in one change.** Adopting sets `<key>` default on and `<key>_ab_split` default off; every family is treated and the kill switch remains. Dropping removes the treated path, the flags and the registry entry. Either change sweeps the tests that pinned the superseded text.

## Adding an experiment

1. Add a registry entry to `EXPERIMENTS` in `tools/boss/engine/feature-flags/src/experiments.rs`: key, lane, required flags, and whether it splits. Registration order is lane priority.
2. Register its flags in `REGISTRY` in `tools/boss/engine/feature-flags/src/lib.rs`, category `experiments`: `<key>` off by default and, when it splits, `<key>_ab_split` on by default. A test fails if the two registries disagree.
3. Read the arm where the behaviour is decided. A prompt or launch change reads it from the arm set the runner resolved before composing, and turns it into a plain option, so the composer and the drivers never see an experiment key. A policy decision (skip a review, admit a post-merge review) resolves the family's arm with the same function and the same family id.
4. Add a section to this page: what it changes, how its effect is read, its guardrail, and when to stop it early. Say in the pull request that it ships inert and which flag makes it live.

## Reading experiment arms and pipeline stages

`boss cost rows --since 14d --experiment batched_search --group-by experiment_arm,pipeline_stage` compares recorded assignments. Filter to the stage or driver the experiment changes, for example `--filter pipeline_stage=filed_work --filter driver=claude`. `boss cost series --since 14d --experiment batched_search --group-by experiment_arm` uses the same selector and filters.

With `--experiment`, the dimension values are `treatment`, `control`, `stopped`, `not_enrolled` (a recorded set without the key), and `not_recorded` (NULL or unreadable launch data). Without it, the value is the entire canonical JSON arm set: overlapping experiments remain distinct combinations, and `{}` differs from `not_recorded`. An entire set can be filtered as one quoted JSON value, for example `--filter 'experiment_arm={"batched_search":"treatment","worker_autocompact":"control"}'`.

The reply's `observed_experiments` lists keys found in launch records in the requested window, including retired keys. Its arm counts cover the window before filters or the row cap. Families are distinct within each arm: revisions share their chain root, review guides use the family of the PR they explain, and other taskless work uses its work-item id. A family with changed recorded assignments can appear in more than one arm, so summing arm family counts is not a distinct-family total. These are observations, not a causal comparison or an outcomes report.

Rows keep at most 5,000 matching runs, newest first. A capped experiment read carries an `experiment_comparison_capped` coverage note: its token comparison covers only retained runs, whereas the observed-experiment counts cover the whole window. Narrow the window or filter to the stage being compared before drawing conclusions.

Pipeline stages use the execution kind first. Reviews use their recorded batch-member role (`pre_merge_review`, `review_supervisor`, `post_merge_review`); an older review with no batch member is `pre_merge_review`. Implementation runs use task provenance to distinguish `filed_work`, `review_finding_revision`, `ci_fix_revision`, `conflict_revision`, `progression_minted_work`, and `post_merge_follow_up`. Review guides are `review_guide`; progression is `progression`; the remaining known kinds keep their names. Unrecognized kinds, roles or provenance are `unclassified`. A review of a CI fix remains a review, rather than inheriting the fix's stage.
