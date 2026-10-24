# Board invalidation routing: runtime versus structural pushes

Implements the "separate runtime and structural invalidations" step of the hub-and-spoke scale track (design `designs/hub-and-spoke-distributed-engine.md`, §2.4 of the investigation). Before this change every `work_invalidated` push made the app re-read the whole product board, however small the change. The hub's measured cost for that is in `investigations/hub-invalidation-measurement-2026-10-22.md` and `work-tree-scale-measurement.md`. Now a push says how much of the board it can have changed, and the frequent one-card kind is answered with a few-card read.

## What a push carries

`TopicEventPayload::WorkInvalidated` has a `class`:

| class        | meaning                                                                                                                                                                  | the app re-reads                                |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------- |
| `structural` | A card may have been added, removed, moved, re-parented or re-statused, a dependency edge may have moved, or the writer does not say. **The wire default.**              | the whole board (`get_work_tree`), as before    |
| `runtime`    | Only runtime fields of the named cards changed: dispatch wait, PR poll state, merge-queue position, doc pointer. No card was added, removed, re-parented or re-statused. | only the named cards (`get_work_runtime_patch`) |

A push from an engine that predates the field has no `class` and an app that predates it ignores the field, so mixed versions behave exactly as they did before the split. An unknown class is read as `structural`.

`get_work_runtime_patch { product_id, item_ids }` answers with `work_runtime_patch { product_id, items, absent }`. Each item is a card plus its `TaskRuntime`, produced by the same row select and the same derivation sequence as the board tree (`work/board_patch.rs`), for the named cards **and the rest of each card's revision chain**, because a root's `has_in_progress_revision`, `ready_for_review` and review section are derived from its revisions and a revision's parent link from its root. Ids that are not live board cards of the product (deleted, archived, moved to another product) come back in `absent` rather than as an error. The reply is stamped with the revision read before the snapshot, like the tree's. Cost is proportional to the chains named, not to the product.

## Which writers are runtime-class

A site is runtime-class only if its write meets the bar above. Anything uncertain stays structural, which costs a full read but can never leave a board stale.

| writer (reason)                                                                           | class      | why                                                                                                                             |
| ----------------------------------------------------------------------------------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------- |
| scheduler drain (`dispatch_wait_changed`, `dispatch_claimed`)                             | runtime    | rewrites one execution's wait reason, or claims it                                                                              |
| merge poller (`pr_poll_state_updated`)                                                    | runtime    | `update_task_pr_poll_state` rewrites only that task's CI, review, mergeability and merge-queue columns                          |
| trunk queue poller (`trunk_queue_state_updated`), merge-queue renumber                    | runtime    | rewrite only the position detail and queue state of one task                                                                    |
| doc pointer (`task_doc_pointer_set`: `set-doc` handler, PR transition, run-done)          | runtime    | only the pointer columns are written                                                                                            |
| spawn, run end, completion, `pr_merged`, CI and conflict ladders, planner, startup sweeps | structural | a card changes status or lane, or revisions are created                                                                         |
| review pass and verdict, `worker_run_done_delivered`                                      | structural | the investigation listed these as runtime (a badge), but their writers also move the card or create a revision in the same flow |
| every frontend mutation except the doc-pointer setter                                     | structural | operator actions can add, remove, move or re-wire                                                                               |

The three review/run-done reasons are the one place this differs from the investigation's reading (§2.2). The measurement below uses the code's classes, not the investigation's.

## A push alone is not enough

Three places could turn a correct class into a stale board, and each is handled.

1. **The engine's per-session queue folds pushes.** A session's outbound queue keeps at most one pending push per topic and used to overwrite the pending one with the newer. That was harmless while every push meant "re-read the board". With classes it is a bug: a structural push followed by a runtime one would reach the app as runtime only, and runtime pushes for different cards would lose all but the last card. Folding now merges two invalidations into one with the stronger class and the union of the named cards (`app/session_queue.rs`, `coalesce_topic_event`). A union that names more cards than a patch is worth (`MAX_RUNTIME_PATCH_ITEMS`, 64) or spans two products is promoted to `structural`.
2. **The app's burst.** The app already collapses pushes for a product into one burst (150 ms quiet window, never more than 1 s after the first). The burst sends the union: if any trigger was structural, names no card, or was a dropped-push resync, the burst reads the whole board, so a runtime push cannot hide a structural one. Only a burst of nothing but named runtime pushes sends a patch.
3. **The patch itself.** The class is a hint, not a promise. The app applies a patch only if every card in it still has the shape it holds: a card it does not hold, or one whose kind, status, project, product, ordinal, parent link or deletion differs, or any id in `absent` that it holds, makes it read the whole board instead, and a rejected patch changes nothing (all cards are checked before any is applied). A misclassified site therefore costs a full read, not a stale board.

Reads are also ordered. A tree and a patch are served concurrently and the patch is far cheaper, so a slow tree that began before a write could land after the patch carrying it and put the older card back. The app therefore never sends a patch while a tree is outstanding for the product; instead it holds the card ids and sends one patch for them when that tree is accepted (the patch reads after the tree, so it cannot undo it, and no second full assembly is paid). Only an expired or failed tree, which leaves the board owed a structural refresh, or a burst past the 64-card cap, turns a runtime-only burst into a tree. It drops the reply of a patch that a later tree request superseded, keeps one patch in flight per product (ids that arrive meanwhile go out together when its reply lands), and retires each outstanding read individually after 30 s. Replies and errors settle only their matching envelope request id; late replies from retired reads are ignored. Snapshot revisions prevent an older tree or patch from overwriting an applied item update. Queued invalidations drain even when a superseded patch reply is dropped. Disconnect forgets all of it.

## What the engine no longer does for a runtime push

A runtime-class mutation (`set-doc` today) skips the product-wide `reconcile_product_executions` write transaction and the scheduler kick: neither can matter when no card moved. Execution-source pushes never paid them.

## Measuring it

On a live engine the counters are in the metrics registry: `board.invalidation.<source>.<class>` (source `mutation`, `batch_mutation` or `execution`), `board.assembly.full` (product-wide assemblies that ran), `board.assembly.runtime_patch` and `board.assembly.runtime_patch_cards`. The `work invalidation fanout` log line carries the same `class`. The ratio that matters is `board.assembly.full` against the push counters.

Two Bazel-run measurements back the claim in this repo. Neither is a capacity measurement of a live hub; both say so.

### Full assemblies avoided: the lifecycle replay

`board_invalidation_replay.rs` replays one steady-state lifecycle period (45 minutes) of the happy-path execution budget from the investigation (§4 there) for 8, 32 and 64 staggered workers. Every push carries the class the real emitter gives it today, the poll-state pushes leave at the merge poller's 60 s sweeps with its 400 ms probe spacing, and the consumer is a Rust model of the app's burst coalescer (150 ms quiet window, 1 s cap). The control run classifies every push structural, which is today's behavior; the saving can therefore only come from the classes.

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_sharding_strategy=disabled \
  --test_filter=board_invalidation_replay --test_output=all --test_arg=--nocapture
```

| workers | pushes (structural / runtime) | bursts = full assemblies before | full assemblies after | patches after | cards per patch | full assemblies saved |
| ------: | ----------------------------: | ------------------------------: | --------------------: | ------------: | --------------: | --------------------: |
|       8 |                       64 / 64 |                             115 |                    56 |            59 |            1.03 |              59 (51%) |
|      32 |                     256 / 256 |                             471 |                   224 |           247 |            1.01 |             247 (52%) |
|      64 |                     512 / 512 |                             932 |                   448 |           484 |            1.02 |             484 (52%) |

Half the pushes are runtime-class, and half of the bursts the app would send are runtime-only, so about half of the full assemblies go away at every fleet size. The saving is not larger than the runtime share of pushes because a runtime push that lands inside the window of a structural one rides along on that burst's full read, and because the review-pass and run-done reasons stay structural.

The table does not model tree latency, and it does not need to for the full-assembly column: a runtime-only burst that fires while a tree is outstanding is held and sent as a patch when the tree is accepted, so it never becomes another tree. Such held bursts merge into one patch per tree, so the patch column is an upper bound; the full-assemblies-after column is what the app sends. (Before this rule, the app answered those bursts with a tree, which at the 72 % and 144 % assembly utilizations below would have eroded most of the saving.)

This is a model of a schedule, not a trace. It uses the investigation's per-execution budget (five poll-state pushes, one queue-position push), has no CI-failure, conflict or revision traffic (all structural, so they only lower the saving), and no operator actions. The investigation's estimate was that runtime pushes are 65 to 80 % of arrivals by count; the code's stricter classes give 50 %, which is what the table uses.

### What a patch costs against a full assembly

`work_tree_production_scale_measurement` (the fixture in `work-tree-scale-measurement.md`: 8,030 tasks, 21,127 executions) now also times `get_work_runtime_patch` for 1, 10 and 100 live cards, and asserts at that cardinality that every patched card is exactly the card the tree produces.

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_sharding_strategy=disabled \
  --test_filter=work_tree_production_scale_measurement --test_output=all --test_arg=--nocapture \
  --test_timeout=2400 --test_env=BOSS_BENCH_SAMPLES=20
```

Local macOS Bazel fastbuild, 20 samples including the cold first fetch (nearest-rank percentiles), host load average about 440 from other jobs: absolute milliseconds are inflated, an unloaded host is several times faster (`investigations/hub-invalidation-measurement-2026-10-22.md` §3.3 puts the quiet-host floor near 2.0 s), and only the ratios carry over.

| answer to one invalidation                       | DB + serialize, p50 / p95 ms | body bytes |
| ------------------------------------------------ | ---------------------------: | ---------: |
| full board assembly (today, every push)          |           8,640.4 / 19,695.0 | 29,863,183 |
| patch of 1 card (1 returned)                     |                  29.1 / 71.5 |      4,185 |
| patch of 10 cards (13 returned: revision chains) |                 29.5 / 205.8 |     54,897 |
| patch of 100 cards (133 returned)                |                237.8 / 381.8 |    562,023 |

A patch costs under 0.4 % of an assembly for one card and under 3 % for a hundred. For comparison the one-record delta that follows a full assembly today costs 6,475 ms in this run (DB, diff and serialize) and ships 3,862 bytes: the wire shrinks and the work does not, which is the gap this change closes for runtime pushes. The engine's patch cap of 64 cards is far below the point where a patch approaches an assembly on the engine side; it is a judgment, not a measured optimum, because this fixture does not time the app's per-card apply cost, which is what grows with the cards named.

### Both together

The replay's counts times these unit costs, as single-core assembly seconds per 2,700 s lifecycle period (assemblies for one product run back to back; the engine's single flight can only make the "before" column smaller):

| workers | before: full assemblies × 8.64 s | after: full × 8.64 s + patches × 0.029 s | before / after at the 2.0 s quiet-host floor |
| ------: | -------------------------------: | ---------------------------------------: | -------------------------------------------: |
|       8 |                     994 s (37 %) |                             486 s (18 %) |                                    8 % / 4 % |
|      32 |                  4,069 s (151 %) |                           1,943 s (72 %) |                                  35 % / 17 % |
|      64 |                  8,053 s (298 %) |                          3,885 s (144 %) |                                  69 % / 33 % |

Over 100 % means the hub could not keep the board current on one core at that load, which is the §2.4 saturation argument. The change roughly halves the assembly load at every fleet size and does not by itself lift the ceiling past what the quiet-host column shows: the remaining half is structural traffic, which is what cached per-item runtimes and the incremental board model address. Treat the table as the arithmetic of a modeled schedule and measured unit costs, not as a measured capacity.

## What is not covered

- Production arrival counts. No engine log was read (`~/Library/Application Support/Boss` is off limits to workers), so the share of runtime pushes in a live engine is the replay's, not an observation. The counters above are how to take it.
- The app's apply cost for a patch against a tree apply. The patch path reuses the single-card update the app already applies for `work_item_updated`, and skips the whole-board bucket rebuild.
- Cached per-item runtimes and the incrementally maintained hub board model, which are the other steps of the same track.
