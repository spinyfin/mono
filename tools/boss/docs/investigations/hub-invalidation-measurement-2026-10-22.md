# Hub invalidation measurement: runtime versus structural board invalidations

- **Date:** 2026-10-22
- **Kind:** investigation (writeup only; no code changes; the follow-up code work is listed at the end for filing)
- **Parent:** the hub-and-spoke project, §3.2 step 10 (hub-side scale work), validating the runtime-invalidation premise in §2.4 of [`hub-and-spoke-distributed-engine-2026-10-21.md`](hub-and-spoke-distributed-engine-2026-10-21.md)
- **Related:** [`../work-tree-scale-measurement.md`](../work-tree-scale-measurement.md) (the existing scale fixture and the `work invalidation fanout` records), [`../designs/fleet-scaling-dynamic-panes-and-team-semantics.md`](../designs/fleet-scaling-dynamic-panes-and-team-semantics.md) (the saturation experiment and serial drain)
- **Citations:** every `file:line` below was read at the `main` this workspace was positioned on (`bc27dd4e`, "Partition rust_test shards with awk instead of a bash loop (#3253)"). Paths are relative to `tools/boss/` unless they start with another top-level directory.

§2.4 of the hub-and-spoke investigation estimates that splitting frequent runtime invalidations from rare structural ones lifts the hub's board ceiling from roughly 30 to 60 workers to roughly 50 to 100, and says in the same breath that this "assumes runtime-only changes are a large share of today's invalidations, and no arrival counts by kind exist yet". This document checks that premise from source, runs the existing Bazel scale fixture to put a current number on the cost of one invalidation, and states exactly what instrumentation and fixture work is still needed to replace the estimate with a measurement.

## TL;DR

- **The premise holds by arrival count, not by vocabulary.** Every `WorkInvalidated` push, whatever its reason, makes the app re-read the whole selected product and the engine assemble the whole product projection; there is no cheaper path for a runtime-only change today. Of the roughly 75 literal reasons the engine can publish across 97 call sites, about half are runtime-class (a card's dispatch wait, PR poll state, merge-queue position, review pass, or doc pointer changed, with the card staying in its lane) and half are structural (a card moves between lanes, or membership or dependencies change). The runtime half is the periodic half: it is what the pollers and the scheduler re-publish, so it dominates what actually arrives.
- **Per execution, the happy path publishes roughly 3 to 4 structural and 6 to 15 runtime invalidations** (section 4). The two largest runtime sources, the merge poller's `pr_poll_state_updated` and the merge-queue renumber, scale with the number of open PRs and the queue depth rather than with the number of running workers, and both arrive in bursts inside one sweep pass.
- **One invalidation costs one product-wide assembly, and the fixture puts that between about 2 s and 6 s at this `main`** (section 3): 5,778 ms p50 / 14,365 ms p95 over 20 samples on the 8,030-task fixture under uncontrolled host load, with a floor of 1,953 ms and six of the twenty samples under 3 s. The 1.3 s that §2.4 cites is the one-record delta probe from a quieter October 12 run (`work-tree-scale-measurement.md:104-110`); this run's one-record delta took 10,371 ms. The app coalesces pushes into at most one refetch per second per product and the engine single-flights overlapping loads, so the hub cannot be made to assemble more than about one tree per assembly duration per product; past that point the board goes stale rather than the hub falling over.
- **At 8/32/64 workers the steady-state lifecycle traffic is about 2, 9 and 17 invalidations per minute on the happy-path budget** (section 5). Whether that saturates one core depends almost entirely on the assembly cost: at the 1.3 s §2.4 uses it is a 5 to 37 % duty cycle and the lifecycle-only ceiling sits near 170 workers; at this run's 5.8 s p50 the 64-worker duty cycle is 164 % and the ceiling sits near 39 workers. On top of the lifecycle traffic sit the bursts: one merge-poller sweep over P active PRs can publish up to P invalidations within seconds, and one merge renumbers every queued PR behind it.
- **§2.4's "30 to 60 workers" is therefore defensible only under the pessimistic assembly cost, and worker count is the least stable axis of the estimate.** A fourfold spread in assembly cost within one 20-sample run moves the lifecycle-only ceiling from roughly 40 to roughly 170 workers, and that cost is set by product size and host load, not by the number of running workers; the burst classes scale with open PRs and queue depth. The §2.4 remedy is still right: making runtime invalidations change-proportional removes the majority of arrivals and all of the burst classes. Step 10 should plan against assembly cost, open-PR count and product size, and re-state the worker figure once the counters below exist.
- **Nothing today records arrival counts by kind, and the scale fixture does not drive invalidations at all.** `work invalidation fanout` is a tracing line (three emitters), not a counter, and the population-timing trace cannot be joined to the invalidation that triggered a load. Section 6 lists the three follow-up code tasks: a classified fanout counter with a join key, a worker-count-parameterised fixture that replays the lifecycle through the real publisher and loader at 8/32/64, and the report that fixture emits.

## 1. Question and scope

§3.2 step 10 is the independent hub-side track: cached per-item runtimes, a runtime/structural invalidation split, batched hook writes and a parallel drain. §2.4 justifies starting with the split by assuming runtime-only changes dominate. This investigation asks three things:

1. Which engine paths publish board invalidations, with what reason, and which of those are runtime-class versus structural-class?
2. What does one invalidation cost the hub today, measured rather than quoted, and what bounds the number of assemblies the hub performs?
3. What would a reproducible fixture at 8, 32 and 64 workers have to do to turn the §2.4 estimate into a measurement, and what instrumentation is missing before it can?

Out of scope: production arrival counts (no engine log was read; `~/Library/Application Support/Boss` is off limits to a worker, and the only historical evidence remains the 19 to 58 full refetches per ten minutes in `work-tree-scale-measurement.md:55`), the live-status summariser budget (§2.4 item 2), and the serial dispatch drain (§2.4 item 3), which is only touched here where the dispatch gauges are the backlog measure the fixture should record.

## 2. What the engine publishes today

### 2.1 Three emitters, one wire shape

Every board invalidation is a `TopicEventPayload::WorkInvalidated { reason, product_id, item_ids }` on the product's topic. Three functions emit the `work invalidation fanout` tracing record and the push:

| Emitter                                               | `source` field   | Who calls it                                                                                                                            | Extra work on the path                                                                                                                    | Citation                                         |
| ----------------------------------------------------- | ---------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------ |
| `publish_work_invalidation`                           | `mutation`       | 32 frontend request handlers (work-item CRUD, product/project settings, dependencies, ideas, planner, attention actions)                | `reconcile_product_executions` over every task in the product inside one write transaction, then a scheduler kick, then the bump and push | `engine/core/src/app/handler_helpers.rs:587-658` |
| `publish_batch_work_invalidation`                     | `batch_mutation` | the two batch-create handlers                                                                                                           | the same reconcile, once per distinct product                                                                                             | `handler_helpers.rs:700-765`                     |
| `BrokerExecutionPublisher::publish_work_item_changed` | `execution`      | 65 engine-internal call sites: scheduler, spawn, completion, merge poller, CI watch, trunk queue poller, conflict ladder, review sweeps | none; bump and push only                                                                                                                  | `engine/core/src/app.rs:2267-2294`               |

The mutation emitters carry a scheduler kick and a product-wide reconcile that the execution emitter does not. §2.4's "move `reconcile_product_executions` off the per-invalidation path" therefore applies to operator and coordinator actions only; engine lifecycle events already skip it. The reconcile reads every task of the product (`engine/core/src/work/executions_runs.rs:991-996`) and has no timing segment of its own, so its cost on an 8k-task product is unmeasured; it is included in section 6.

What does **not** invalidate the board, and matters for the §2.4 arithmetic:

- **Hook events.** A worker hook (two per tool call) updates the in-memory live-worker registry, stamps the dispatcher counters, and persists `transcript_path` once per run; later hooks are a no-op `UPDATE` (`engine/core/src/app/worker_events.rs:491-560`, `live_status_loop.rs:170-189`). No `WorkInvalidated` is published.
- **Live status.** The summariser and the pane-state sweeps publish a separate live-worker-state push, which the app applies to its `LiveWorkerStateStore` without touching the board (`app-macos/Sources/ChatViewModel+EventHandling.swift:302`). The card's agent indicator is therefore already change-proportional; it is not part of the problem §2.4 describes.
- **Execution topic events.** `ExecutionPublisher::publish` goes to the per-execution topic, which only an open execution viewer subscribes to (`app.rs:2246-2265`).

### 2.2 Reason inventory, classified

The classification rule used here: **structural** means the push can move a card between lanes, add or remove a card, or change a dependency edge, so a board that only patched the one card's runtime fields could be wrong; **runtime** means the card stays where it is and only fields that a per-card patch could carry (dispatch wait text, CI/review/merge-queue indicators, review-pass badge, doc pointer, live attempt) changed. The split is a reading of what each writer changes, not a label the engine attaches; the engine has no such field, which is one of the gaps in section 6.

**Execution emitter (65 call sites).** Grouped by the subsystem that owns the write:

| Subsystem                                 | Reasons (representative)                                                                                                                                                                                     | Class                                             | Cadence driver                                                                                                                      | Citation                                                                                                                              |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| Scheduler drain                           | `dispatch_wait_changed`, `dispatch_claimed`                                                                                                                                                                  | runtime                                           | every drain pass, for each queued execution whose wait reason or blocker changed; once per claim                                    | `engine/core/src/coordinator/scheduler.rs:754-762`, `:1036`, `:1783`, `:2204`                                                         |
| Spawn                                     | `execution_started_auto_advance`, `execution_run_failed`, `execution_start_failed`, `merge_conflict_already_resolved`                                                                                        | structural                                        | once per spawn                                                                                                                      | `coordinator/execution.rs:1925-1944`, `:2541`; `coordinator/run.rs:620-628`                                                           |
| Completion                                | `execution_run_completed`, `worker_pr_completed`, `worker_pr_merged`, `worker_run_done_delivered`, `worker_no_op_completed`, `worker_completion_refused`, `worker_failure_finalized`, `worker_force_stopped` | structural                                        | once or twice per run end                                                                                                           | `coordinator/run.rs:1102`; `completion/pr_transition.rs:695-710`; `completion/run_done_declaration.rs:465`; `completion/no_op.rs:218` |
| Doc pointers                              | `task_doc_pointer_set`, `design_doc_pointer_set`                                                                                                                                                             | runtime                                           | once per investigation or design run                                                                                                | `completion/pr_transition.rs:767`, `:844`; `run_done_declaration.rs:1075-1083`                                                        |
| Merge poller                              | `pr_poll_state_updated`, `pr_merged`, `pr_closed_unmerged`, `merge_queue_lane_reasserted`, `comments_reopened_on_pr_closed_unmerged`, `stranded_remediation_recovered`, `reviewer_fallback_*`                | runtime except `pr_merged` / `pr_closed_unmerged` | one sweep pass every 60 s over every tracked PR plus a 40 s adaptive tier for hot PRs; publishes only when the stored state differs | `merge_poller/sweep.rs:2384-2420`, `:2581`, `:2676`; `merge_poller/schedule.rs:40-41`, `:58-80`; `app/server.rs:1705`                 |
| Merge queue renumber / trunk queue poller | `trunk_queue_state_updated`, renumber events                                                                                                                                                                 | runtime                                           | one push per queued PR whose position changed, after every merge ahead of it; trunk tier 15 s to 30 s                               | `merge_queue_renumber.rs:75-95`; `trunk_queue_poller.rs:1045-1065`, `:309-310`                                                        |
| CI watch and remediation                  | `ci_revision_in_flight`, `blocked_ci_failure`, `blocked_ci_failure_exhausted`, `merge_queue_ci_attempt_superseded`, `ci_failure_resolved`                                                                    | structural                                        | per CI failure                                                                                                                      | `ci_watch.rs:217`, `:429`, `:535`, `:745`; `completion/execution_started.rs:455`                                                      |
| Review pipeline                           | `pr_review_pass_completed`, `pr_review_pass_revision_created`, `pr_review_deletion_signoff`, `review_verdict_applied`, `review_admission_recovered`, review-guide reasons                                    | runtime (badge) except revision creation          | per review pass                                                                                                                     | `completion/finalize_passes.rs:1442-1554`; `review_verdict_apply_sweep.rs:37`; `merge_poller/deferred_review_admission.rs:77-146`     |
| Conflict ladder / watch                   | `blocked_merge_conflict`, `merge_conflict_resolved`, ladder recoveries                                                                                                                                       | structural                                        | per conflict                                                                                                                        | `conflict_watch.rs:416-972`; `conflict_ladder.rs:856`, `:997`                                                                         |
| Startup sweeps                            | ghost-active demotion, orphaned ladder recovery, `worker_readopted`                                                                                                                                          | structural                                        | engine start only                                                                                                                   | `app/server.rs:1073`, `:1139`; `app/readoption.rs:611`                                                                                |
| Planner                                   | `planner_run_changed`                                                                                                                                                                                        | structural                                        | per planner run                                                                                                                     | `populator.rs:981`                                                                                                                    |

**Mutation emitter (32 call sites).** `work_item_updated` (four handlers), `work_item_deleted`, `project_*` (created, planned, released, unpopulated, reordered, postmortem, design doc), `product_*` settings, `dependency_added` / `dependency_removed`, `idea_*`, `attention_group_actioned`, `operator_question_answered`, `automation_task_created`, `external_tracker_updated`, `doc_comment_revise_created`, `deferred_scope_attention_converted`. All but the product-settings and idea reasons are structural; the operator is the cadence driver, so they are rare and bursty rather than worker-proportional.

Counting literal reason strings gives 43 on the execution emitter (plus a handful passed through variables, including the scheduler's two) and 27 on the mutation emitter. By the rule above about 24 of the execution-side reasons and about 10 of the mutation-side ones are runtime-class, so the vocabulary is roughly half and half. The count that matters is arrivals, not vocabulary: the runtime reasons are the ones the pollers and the scheduler re-publish on a cadence, and section 4 estimates arrivals per execution.

### 2.3 What the records can and cannot tell you today

The `work invalidation fanout` record carries `source`, `reason`, `product_id`, `item_count` and `topic_count` (`handler_helpers.rs:596-603`). It is `tracing::info!` into the engine log: there is no counter in the metrics registry (`dispatch_metrics.rs` has queue and drain gauges only), no runtime/structural field, and no identifier that the app's refetch or the engine's `GetWorkTree` trace could carry back. The population-timing trace joins on `(request_id, fetch_seq)` with the app (`engine/core/src/population_timing.rs:48-56`), but the app's refetch burst does not record which pushes it coalesced, so a load cannot be attributed to the invalidation batch that caused it. Grouping the log by source and reason gives arrival counts, as `work-tree-scale-measurement.md:55` says, but only from a live engine's log, which this investigation did not read.

## 3. What one invalidation costs

### 3.1 The refetch path

1. The engine bumps `work_revision` and publishes one push per subscribed topic (`handler_helpers.rs:620-655`).
2. The app ignores the reason. A push for the product on screen joins that product's pending burst with scope `.invalidation`: the whole tree, the deferred-scope list, `list_executions` for every invalidated item with an open viewer, and the background-work poll. The burst timer waits 150 ms after the latest push and never more than 1 s after the first (`app-macos/Sources/ChatViewModel+InvalidationRefetch.swift:22-38`, `:100-125`). A push for an off-screen product fetches only the lazy lists.
3. The engine serves `GetWorkTree` through a per-product single flight keyed on `(work_revision, sqlite data_version)`: overlapping requests for the same product share one load, but nothing is cached across loads, and every invalidation bumps the revision, so each burst that reaches the engine assembles the product again (`engine/core/src/app/work_tree_loads.rs:13-48`; `engine/single-flight/src/lib.rs:1-6`).
4. Assembly is product-wide: all tasks, chores, runtimes, dependencies, review state, doc pointers, then the session-local delta diff and serialization (`population_timing.rs:14-30`; `work-tree-scale-measurement.md`, "Implemented reconciliation").

So the number of assemblies per product per second is bounded above by `min(1 / coalescing max-wait, 1 / assembly duration)`: at most one per second by the app, and at most one per assembly duration by the single flight. When invalidations arrive faster than the assembly duration the hub runs assembly back to back on one core per product and the board lags by one to two assemblies; it does not amplify.

### 3.2 Measured assembly cost at this `main`

Reproduction, run from the workspace root (the documented command in `work-tree-scale-measurement.md:9` predates the 11-way sharding of `engine_lib_test`; with sharding on, a lone `--test_filter` only reaches the shard the test hashes into, so sharding is disabled for the run):

```sh
bazel test //tools/boss/engine/core:engine_lib_test \
  --test_sharding_strategy=disabled \
  --test_filter=work_tree_production_scale_measurement \
  --test_output=all --test_arg=--nocapture --test_timeout=1800 \
  --test_env=BOSS_BENCH_SAMPLES=20
```

Local macOS Bazel fastbuild, uncontrolled host load, 20 sequential samples including the cold first fetch, percentiles by nearest rank. The run reported here was served by Bazel's action cache (`(cached) PASSED in 164.5s`): the test binary and test environment were byte-identical to an earlier invocation of the same command on this host at this `main`, so the timings below are that earlier execution's, replayed. Bazel's caching was not bypassed to force a fresh run; the variance within the 20 samples (section 3.3) is a larger effect than any re-run would be. The fixture is the deterministic 8,030-task / 21,127-execution one from `engine/core/src/work/tests/work_tree_scale_tests.rs:6-51`.

| Metric                                               |                              Value |
| ---------------------------------------------------- | ---------------------------------: |
| Returned task/chore records                          |                              7,585 |
| Serialized tree body, bytes                          |                         29,863,183 |
| DB assembly + serialization p50, ms                  |                          5,777.619 |
| DB assembly + serialization p95, ms                  |                         14,364.902 |
| `db.ai_review_state` p50 / p95, ms                   |                454.413 / 1,203.360 |
| Runtime queries per fetch                            |                             22,755 |
| 1 / 10 / 100-record delta: DB + diff + serialize, ms | 10,371.066 / 5,000.012 / 4,791.398 |
| 1 / 10 / 100-record delta body, bytes                |           3,862 / 37,504 / 374,269 |

The byte counts are exact. The three delta bodies are identical to the byte to the October 12 run in `work-tree-scale-measurement.md`, and the snapshot carries the same 77 bytes of update metadata (29,863,260 against a 29,863,183 tree body). The full tree body is 9,538 bytes larger than October 12's 29,853,645, consistent with projection fields added to the work tree since; it is not measurement variance, since every sample serializes the same fixture. The timings are not: the October 12 post-rebase run reported 1,852.617 / 3,440.486 ms p50 / p95 for the full fetch and 1,339.380 / 1,327.944 / 1,399.412 ms for the three delta probes, so this run is roughly 3× slower at the median and 8× slower on the one-record delta.

The delta rows are the one-invalidation cost that matters for §2.4: a single changed record still costs a full assembly plus diff (10.4 s here, 1.3 s on October 12, in each case in the band of that run's full fetch), and the wire packet is the only thing that shrinks. That is the measured form of §2.4's "today's delta only shrinks the wire packet after a full product-wide assembly".

### 3.3 The spread inside one run

The twenty samples are bimodal rather than noisy around a mean. Sorted, they run 1,953 / 1,992 / 2,102 / 2,349 / 2,598 / 2,871 / 3,675 / 4,334 / 5,076 / 5,778 / 6,067 / 6,156 / 6,808 / 7,012 / 7,040 / 7,440 / 7,752 / 11,699 / 14,365 / 18,892 ms. Samples 2 to 7 in arrival order all landed between 1.95 s and 2.9 s, which is the October 12 band; samples 8 to 19 landed between 3.7 s and 18.9 s. The `db.ai_review_state` segment tracks the same shape (197 ms to 1,267 ms), so the slow samples are the whole fetch slowing down, not one query regressing. Nothing in the fixture changes between samples; the only variable is host load. For the capacity arithmetic in section 5 this means a single "assembly cost" does not exist: the quiet-host floor is about 2.0 s, the loaded-host median is about 5.8 s, and §2.4's 1.3 s is the quiet-host delta probe. Section 5 carries all three.

## 4. Per-execution invalidation budget, from source

The fixture cannot produce this (section 6), so the budget is read off the call sites in section 2.2 for one task execution on the happy path: dispatched from `todo`, runs, opens a PR, passes one automated review, CI goes green, merges through the merge queue, and the task lands in `done`.

| Phase               | Reason(s)                                                                     | Class                                   | Count per execution                                                                                                                                                                                                                 |
| ------------------- | ----------------------------------------------------------------------------- | --------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Queued              | `dispatch_wait_changed`                                                       | runtime                                 | 0 when a slot is free; otherwise one per change of wait reason. The chain-serialized reason embeds the queue length and the running sibling's label, so it changes every time the queue ahead moves (`scheduler.rs:764-800`)        |
| Claim               | `dispatch_claimed`                                                            | runtime                                 | 1                                                                                                                                                                                                                                   |
| Spawn               | `execution_started_auto_advance`                                              | structural                              | 1                                                                                                                                                                                                                                   |
| Working             | none (hooks and live status do not invalidate)                                |                                         | 0                                                                                                                                                                                                                                   |
| Run end             | `execution_run_completed`, `worker_pr_completed`, `worker_run_done_delivered` | structural (first two), runtime (third) | 2 to 3                                                                                                                                                                                                                              |
| PR open, CI running | `pr_poll_state_updated`                                                       | runtime                                 | 1 per sweep in which CI state, review state, mergeability, merge-state status, head SHA or GitHub's `updated_at` moved (`work/pr_flow.rs:1551-1573`); typically 3 to 8 over a PR's life, every push or review comment counting once |
| Review              | `pr_review_pass_completed`, `review_verdict_applied`                          | runtime                                 | 2 (one pass, one verdict apply); more with revisions                                                                                                                                                                                |
| Merge queue         | `trunk_queue_state_updated` or renumber events                                | runtime                                 | 1 on enqueue plus 1 per merge ahead of it                                                                                                                                                                                           |
| Merge               | `pr_merged`, then `worker_pr_merged` if a completion recheck runs             | structural                              | 1 to 2                                                                                                                                                                                                                              |
| Doc pointer         | `task_doc_pointer_set`                                                        | runtime                                 | 0 or 1                                                                                                                                                                                                                              |

**Totals:** 3 to 4 structural, 6 to 15 runtime, so a runtime share of roughly 65 to 80 % by count on the happy path. The §2.4 assumption is therefore consistent with the code, with two qualifications: the runtime count is dominated by PR-side churn (`pr_poll_state_updated`, queue position) that scales with open PRs and queue depth rather than with running workers, and the `pr_updated_at` arm of the poll's changed-check means any GitHub-side activity on a PR costs one full board assembly on the next sweep even when nothing the card shows has changed.

**Failure and revision paths add structural traffic.** A CI failure adds `blocked_ci_failure` or `ci_revision_in_flight`, a revision execution (its own full budget), and `ci_failure_resolved`; a merge conflict adds the ladder's events. These are correct structural invalidations and are not what the split targets.

## 5. Capacity model at 8, 32 and 64 workers

The brief asked for a fixture at these sizes. The fixture that would produce measured numbers does not exist yet (section 6), so this section computes what it would be expected to show from the constants above, with the assumptions stated so the fixture can falsify them.

**Assumptions.** One large product (the fixture's 8,030 tasks); every worker is on the happy path with budget E = 12 invalidations per execution (4 structural, 8 runtime) over a lifecycle L = 45 min from claim to merge; the merge poller's 60 s sweep is the only burst source; assembly cost A taken at three values, 1.3 s (the §2.4 figure), 2.0 s (this run's quiet-host floor) and 5.8 s (this run's p50), per section 3.3; the app's coalescing holds (at most one refetch per second per product).

| Workers N | Lifecycle invalidations per minute (N·E/L) | Mean inter-arrival | Duty cycle at A = 1.3 s | Duty cycle at A = 2.0 s | Duty cycle at A = 5.8 s | Structural / runtime per minute |
| --------: | -----------------------------------------: | -----------------: | ----------------------: | ----------------------: | ----------------------: | ------------------------------: |
|         8 |                                        2.1 |               28 s |                     5 % |                     7 % |                    21 % |                       0.7 / 1.4 |
|        32 |                                        8.5 |              7.0 s |                    18 % |                    28 % |                    82 % |                       2.8 / 5.7 |
|        64 |                                       17.1 |              3.5 s |                    37 % |                    57 % |                   164 % |                      5.7 / 11.4 |

Duty cycle is arrival rate × A before coalescing; above 100 % the single flight queues assemblies back to back and the board lags. The ceiling, one assembly per assembly duration, is reached at 60 / A arrivals per minute: 46 per minute at 1.3 s, 30 at 2.0 s, 10.4 at 5.8 s. On the happy-path budget that corresponds to roughly 173, 112 and 39 concurrent workers respectively (N = 60·L / (A·E)).

**Reading.** The answer to "how many workers before board assembly saturates a core" is set by A, and A moved fourfold inside one 20-sample run. At the quiet-host figures (1.3 s to 2.0 s) lifecycle traffic at 64 workers is a fraction of one core and the lifecycle-only ceiling is well above 100 workers, so §2.4's 30 to 60 is pessimistic. At the loaded-host median (5.8 s) 32 workers already run the assembler at 82 % and 64 workers are past saturation, and the lifecycle-only ceiling of about 39 workers lands inside §2.4's range. Both numbers come from the same fixture at the same `main`; the laptop hub in §2.4's scenario is by construction a loaded host (it also runs the app, the live-status summariser and whatever local workers remain), so the pessimistic column is the one to plan against until a release-build measurement on a representative host exists. Either way the worker count is the derived quantity; A and the arrival rate are the ones to measure.

**What does reach the ceiling.**

- **Sweep bursts.** One merge-poller pass probes every tracked PR in sequence and publishes `pr_poll_state_updated` for each one that moved (`merge_poller/sweep.rs:2384-2420`). With P open PRs whose CI is still running, a pass can publish up to P invalidations within a few seconds, every 60 s. The app coalesces them into one or two refetches, but the engine assembles once per burst regardless of P, so this costs one assembly per sweep, not P. Where it hurts is the overlap: a burst landing during an assembly in flight queues a second one behind the single flight.
- **Queue renumbers.** Every merge renumbers every queued PR behind it, one push each (`merge_queue_renumber.rs:75-95`). With a queue of Q, each merge is Q pushes; again coalesced to one assembly.
- **Operator batch actions.** Batch creates and planner releases publish once per product but run the product-wide reconcile first (`handler_helpers.rs:715-738`); they are rare.
- **Product size.** A is proportional to the product, not to N. A product twice the fixture's size halves every figure in the table.

So the honest restatement of §2.4 is: with board maintenance as it is today, the hub's board cost is `(sweeps + merges + lifecycle events) × A(product size, host load)`, with worker count entering only through the lifecycle term. At the loaded-host A that term alone saturates near 40 workers; at the quiet-host A the burst terms saturate first. The runtime/structural split removes the majority of arrivals and all of the burst classes, because every burst class is runtime by the rule in section 2.2. The structural residue in the table (0.7 to 5.7 per minute) stays below the ceiling at every size and every A, but at 64 workers and A = 5.8 s it is still a 55 % duty cycle, which is why §2.4 also lists cached per-item runtimes and, eventually, an incrementally maintained board model rather than the split alone.

**DB writes.** Per worker: two hook frames per tool call, each one `WorkDb` read for the driver slug and one usually-no-op `UPDATE` of `transcript_path` (§1.5 of the parent investigation; `worker_events.rs:491-560`); per PR per sweep: one unconditional `UPDATE` of the poll timestamps plus one conditional state `UPDATE` (`pr_flow.rs:1536-1573`); per drain pass: one conditional `UPDATE` per queued execution (`work/dispatch.rs:1678-1698`). At 64 workers making a tool call every 10 s that is about 13 small writes per second plus about one per second of poll stamps, inside the "10 small writes/s" §2.4 already budgets for 100 workers. The mutation-path reconcile is the one write of unknown cost; it reads the whole product under the writer connection.

**Dispatch backlog.** The gauges exist and are the right measure: `dispatch.queue_depth.{main,automation,review}`, `dispatch.queue_oldest_wait_seconds.*`, `dispatch.completed` and `dispatch.drain_pass_duration_ms`, sampled per drain pass and flushed every 30 s (`engine/core/src/dispatch_metrics.rs:1-58`). They are not exercised by the scale fixture, which never runs the coordinator, so no number is reported here. The fixture in section 6 should run the drain against its seeded queue and report them.

## 6. Measurement gaps and the follow-up code work

Three things stand between this document and a measured §2.4. They are listed here for the user to file; this investigation makes no code change.

### 6.1 Source/reason/class fanout counters with a join key

- Give `publish_work_item_changed`, `publish_work_invalidation` and the batch variant an explicit class (`runtime` or `structural`) chosen at the call site, defaulting nothing: the compiler should force each of the 97 sites to say which it is. The section 2.2 table is the proposed initial assignment.
- Register counters keyed by `(source, class)` and by `reason` in the metrics registry next to the dispatch gauges, so arrival counts survive restarts and are queryable like every other engine metric, instead of living only in the tracing line.
- Stamp the push's `revision` into the app's refetch request (the envelope already carries it) and record it in the population-timing trace, so an assembly can be attributed to the invalidation batch that triggered it and the sweep-burst overlap in section 5 can be seen directly.
- Add a population-timing segment for `reconcile_product_executions` on the mutation path.

### 6.2 A worker-count-parameterised invalidation fixture

The existing fixture mutates rows directly and measures assembly (`work_tree_scale_tests.rs:151-179`). The §2.4 question needs a fixture that drives the real publishers:

- Seed the 8,030-task product plus N active executions with runs and open PRs, for N in {8, 32, 64} via an env var in the style of `BOSS_BENCH_SAMPLES`.
- Replay the section 4 lifecycle for each execution through `BrokerExecutionPublisher` and the scheduler's wait-reason path, with a scripted merge-poller sweep over all N PRs every simulated 60 s, so bursts occur as they do in production.
- Model the app's coalescer in the test (150 ms window, 1 s max wait) feeding the engine's `WorkTreeLoads` single flight, and record per class: pushes published, refetches issued, assemblies performed, assembly ms, bytes, SQLite `data_version` delta (writes), and the dispatch gauges from a real drain over the seeded queue.
- Run under `bazel test` only, like today's fixture, with the same "no production rows" rule.

### 6.3 The report

Have the fixture print one table per N in the shape of section 5, and check it into `docs/` beside `work-tree-scale-measurement.md` with the reproduction command, so step 10's cached-runtime and split work has a before number to compare against.

## 7. Verdict on the §2.4 premise

Runtime-class invalidations are the majority of what the engine publishes by happy-path count per execution (roughly 65 to 80 %, with the vocabulary itself split about evenly), so the §2.4 split targets the right class and the cheap measures remain the right first step for step 10. The premise holds.

The worker-count figure is weaker than the premise. One invalidation costs one product-wide assembly whatever its reason, and that assembly measured 5.8 s p50 on the 8k-task fixture at this `main` under load, with a 2.0 s quiet-host floor, against the 1.3 s §2.4 took from the October 12 delta probe. Across that range the lifecycle-only ceiling runs from about 39 to about 173 concurrent workers, so "30 to 60" is what a loaded laptop hub would show and is roughly three times too low for a quiet one. The arrivals that push a hub toward the ceiling fastest are the PR-poll and merge-queue bursts, which scale with open PRs and queue depth rather than with running workers. Until the counters in 6.1 exist and the fixture in 6.2 has run, "30 to 60 workers, lifted to 50 to 100" should be cited as an estimate whose dominant uncertainty is the assembly cost on the hub's actual host, and step 10 should plan against assembly cost, open-PR count and product size, with the worker figure derived from those.

## Appendix: raw fixture output

Lines printed by the fixture, in order (all other test output omitted):

```text
scale sample=0 fetch_db_and_serialize_ms=7012.476 ai_review_state_ms=1169.467
scale payload_bytes=29863183 description_bytes=24029280 returned=7585
scale runtime_queries=Some((2549.1119160000003, Some(7585), Some(22755)))
scale done=6640 hidden_terminal_revisions=1747
scale candidate_raw_records=1 bytes=3758
scale candidate_raw_records=10 bytes=37320
scale candidate_raw_records=100 bytes=373285
scale sample=1 fetch_db_and_serialize_ms=7039.929 ai_review_state_ms=753.462
scale sample=2 fetch_db_and_serialize_ms=2598.342 ai_review_state_ms=197.191
scale sample=3 fetch_db_and_serialize_ms=2871.020 ai_review_state_ms=342.923
scale sample=4 fetch_db_and_serialize_ms=2348.987 ai_review_state_ms=307.929
scale sample=5 fetch_db_and_serialize_ms=1992.400 ai_review_state_ms=210.337
scale sample=6 fetch_db_and_serialize_ms=2101.617 ai_review_state_ms=218.789
scale sample=7 fetch_db_and_serialize_ms=1953.203 ai_review_state_ms=203.138
scale sample=8 fetch_db_and_serialize_ms=18891.946 ai_review_state_ms=1267.075
scale sample=9 fetch_db_and_serialize_ms=5777.619 ai_review_state_ms=629.194
scale sample=10 fetch_db_and_serialize_ms=4333.808 ai_review_state_ms=454.413
scale sample=11 fetch_db_and_serialize_ms=3675.082 ai_review_state_ms=283.891
scale sample=12 fetch_db_and_serialize_ms=7439.899 ai_review_state_ms=700.621
scale sample=13 fetch_db_and_serialize_ms=11698.784 ai_review_state_ms=1049.365
scale sample=14 fetch_db_and_serialize_ms=14364.902 ai_review_state_ms=1203.360
scale sample=15 fetch_db_and_serialize_ms=6808.357 ai_review_state_ms=477.000
scale sample=16 fetch_db_and_serialize_ms=5076.271 ai_review_state_ms=596.078
scale sample=17 fetch_db_and_serialize_ms=7752.107 ai_review_state_ms=500.204
scale sample=18 fetch_db_and_serialize_ms=6156.359 ai_review_state_ms=370.931
scale sample=19 fetch_db_and_serialize_ms=6067.253 ai_review_state_ms=452.201
scale samples=20 fetch_db_and_serialize_ms p50=5777.619 p95=14364.902; ai_review_state_ms p50=454.413 p95=1203.360
scale snapshot_update_bytes=29863260
scale changed_records=1 delta_update_bytes=3862 db_diff_serialize_ms=10371.066
scale changed_records=10 delta_update_bytes=37504 db_diff_serialize_ms=5000.012
scale changed_records=100 delta_update_bytes=374269 db_diff_serialize_ms=4791.398
```
