# Work-tree scale measurement

This PR implements session-local work-tree deltas and pins the recursive review-family lookup to the existing parent index. Both take effect when the updated engine and app are deployed; older clients continue receiving full trees. Invalidation diagnostics take effect on engine deploy; the benchmark runs only under Bazel. No production database, identifiers, descriptions, or other row contents belong in this report or its fixture.

## Reproduction

Run the deterministic fixture through the real database assembly and serialization path:

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_filter=work_tree_production_scale_measurement --test_output=all --test_arg=--nocapture --test_timeout=1800 --test_env=BOSS_BENCH_SAMPLES=20
```

For an explicitly authorized consistent SQLite snapshot, change the filter to `work_tree_authorized_snapshot_measurement` and add `--test_env=BOSS_BENCH_SNAPSHOT=/absolute/path/to/snapshot.db`. The measurement opens that source with `immutable=1` and read-only flags, copies it into the test's private temporary directory with `VACUUM INTO`, and initializes only the copy. Tests never contact the production engine. The optional snapshot test emits aggregate counts, sizes, and durations only.

The main product in the synthetic fixture has 8,030 tasks (6,640 done, 445 archived, 350 in review, 321 todo, 197 blocked, 77 active), 131 projects, five products, 21,127 executions, 19,703 runs, 4,131 proposals, 786 review batches, and 1,818 verdicts. It checks every table count and foreign-key integrity. The fixture also includes 2,000 done tasks, guide series, source comparisons, and failed guide attempts in a separate product to exercise lookup isolation. Assumptions: one large product, one unrelated history product, three empty products, fixed 3 KiB briefs, one revision per four tasks, completed review executions, and clean verdicts. It deliberately makes no claim to reproduce production's revision topology, description distribution, or event arrival process.

Ordinary regression runs collect one full-tree sample and the delta probes; the benchmark command requests twenty sequential full-tree samples including the first fetch, followed by those probes. Percentiles use nearest rank. `fetch_db_and_serialize_ms` measures `get_work_tree_instrumented` plus JSON serialization; it excludes request queuing, Unix-socket transport, and app decoding. `ai_review_state_ms` uses the existing engine population trace. The original raw-record sizes below are sizing experiments. The revised synthetic harness also changes 1, 10 and 100 database task names, assembles the real projections, and measures serialized production update packets including sequence metadata and all changed collections.

## Baseline measurements

Measured on October 11 using the local macOS Bazel fastbuild configuration. Each fixture ran separately, with twenty sequential samples and no cache overrides. Host load was uncontrolled; these are baseline measurements, not a comparison with the production engine's build or load. These historical results predate the implementation; the matched synthetic before/after run is reported separately below.

| Metric                                     |        Synthetic fixture | Authorized snapshot, largest product |
| ------------------------------------------ | -----------------------: | -----------------------------------: |
| Returned task/chore records                |                    7,585 |                                5,372 |
| Serialized tree body, bytes                |               29,853,645 |                           25,990,229 |
| Task descriptions, UTF-8 bytes             |               24,029,280 |                           19,491,676 |
| DB assembly + serialization p50, ms        |                6,226.386 |                           18,485.925 |
| DB assembly + serialization p95, ms        |               13,724.951 |                           37,227.183 |
| `db.ai_review_state` p50, ms               |                3,512.576 |                           14,399.894 |
| `db.ai_review_state` p95, ms               |                7,663.381 |                           26,835.443 |
| Runtime query count per fetch              |                   22,755 |                               16,430 |
| First 1 / 10 / 100 raw task records, bytes | 3,758 / 37,320 / 373,285 |               603 / 11,744 / 286,059 |

Before this change, a non-coalesced selected-product invalidation caused a tree body of approximately 26 MB on the snapshot, even when it carries one item ID. The table excludes protocol-envelope bytes and all queue/socket time. The first-N record sizes are illustrative slices in query order, not typical patch sizes or an implemented delta path. Descriptions account for approximately 75% of the snapshot's tree body.

The initial concurrent two-fixture benchmark exceeded its 600-second window on the snapshot (the synthetic test passed). Running the measurements independently completed in 158 seconds for the synthetic fixture and 407 seconds for the snapshot. The reproduction command allows an explicit 1,800-second window for twenty slow baseline fetches; the repository's ordinary test timeout is unchanged, and its one-sample regression passed in the complete test suite. All fixture integrity and review-state assertions remain enabled in both modes.

## Original refetch sources

Source inspection finds 32 mutation callers of `publish_work_invalidation`, plus batch mutation and execution publishers. Before this change, the selected-product app handler ignored `item_ids` for board refresh and called `refetchForInvalidation` for either a matching product topic or matching payload product. The refresh still assembles the complete tree and requests attention/planner data, but the updated app opts into snapshot/delta responses for every work-tree request, including CRUD, product-list, planner and invalidation refreshes. Existing 150 ms coalescing is not a correctness or bandwidth solution.

| Source                                                                                          | Items carried             | Baseline board consequence                 |
| ----------------------------------------------------------------------------------------------- | ------------------------- | ------------------------------------------ |
| Execution publisher: lifecycle, review, PR state changes                                        | One                       | Entire selected product                    |
| Work-item CRUD, document pointer, product defaults, external tracker, automation-created task   | Usually one               | Entire selected product                    |
| Dependency add/remove                                                                           | Two endpoints             | Entire selected product                    |
| Project postmortem                                                                              | Project and task          | Entire selected product                    |
| Project reorder, planner release/unpopulate, operator answer, idea graduation, attention action | Variable affected-ID list | Entire selected product                    |
| Batch work mutation                                                                             | IDs grouped by product    | Entire selected product                    |
| Comment artifact invalidation                                                                   | One artifact; no product  | Open comment layer; no direct tree refresh |

Additional full-tree paths include CRUD result handlers, planner completion, `products_list` (including a second cold-start fetch), reconnect/resync, product selection, and manual refresh. Fixing only the `workInvalidated` arm would leave other amplification paths.

New `work invalidation fanout` records report source, reason, product correlation, item count, and requested topic count (before de-duplication) at the mutation, batch, and execution publishers without logging item contents. Group these by source/reason to obtain arrival counts; join with the existing population-timing and frontend-frame diagnostics to measure actual refetches and bytes. Publisher counts are not equivalent to app refetch counts because subscriptions and existing coalescing intervene. Historical event frequency cannot be recovered from a state-only SQLite snapshot. Engine logs from October 9–11 showed 19–58 full-tree refetches per ten-minute bucket and five slow-subscriber disconnections in about eleven minutes; that remains the only historical evidence.

### Measurement boundary

The source table is a static inventory, not production arrival counts. The deterministic mutation benchmark measures database assembly and serialized update packets; it does not claim per-topic event rates or socket delivery latency. A state-only snapshot cannot establish those rates.

## Visibility and payload

The authorized October 11 snapshot contains 8,108 task rows: 6,702 done, 461 archived, 334 in review, 333 todo, 201 blocked, and 77 active. Across all products, done descriptions occupy 26,125,681 characters. These are newer counts than the October 9 fixture specification.

For the largest product, non-deleted rows include 4,938 done and 434 open tasks; 30 archived rows are already excluded by the tree queries. Of the non-archived rows, 2,395 are terminal revisions hidden as standalone board cards (their summaries appear on parent cards). They account for 44.6% of the 5,372 non-archived rows, leaving at most 2,977 standalone cards before project and view filters. Their descriptions alone occupy 9,871,883 characters. This is an upper bound on removable detail, not permission to discard revision projections.

The Done lane groups history into collapsible sections and still permits expanding Earlier. It has no history cutoff. Preserve this behavior: introduce detail/history loading before removing information. `WorkCardPopover` also reads descriptions from the current task object, so clearing descriptions without adding detail loading would silently break that view.

## Query-plan diagnosis

A read-only SQL experiment over 5,054 PR-bearing/revision IDs on the authorized snapshot found the recursive family step using `SEARCH t USING AUTOMATIC PARTIAL COVERING INDEX (kind=?)`, rather than the existing `idx_tasks_parent_task_id`. The same plan appears after excluding design, postmortem, and investigation kinds to match the engine review selector (4,914 IDs). This repeatedly visits revision candidates for each family node. The correlated verdict lookup additionally sorts candidates; the completed-execution lookup chooses `work_executions_ready_idx (status=?)` and sorts.

The timing experiment used the broader 5,054-ID diagnostic input. Three standalone Python/SQLite baseline executions of the extracted current-head query took 7,276 / 15,812 / 30,341 ms. A diagnostic query forcing the existing parent index took 1,693 / 688 / 1,332 ms. These are noisy, non-interleaved query experiments during local compilation, not engine before/after benchmarks and not production performance promises. The implementation now uses INDEXED BY idx_tasks_parent_task_id on the recursive child join. An engine-SQLite regression pins the parent-key lookup plan and compares the original and indexed family rows, including cyclic ancestry. Existing current-head verdict tests continue to exercise the unchanged selection rules. No new index is added.

## Implemented reconciliation

Each opted-in connection retains one complete serialized product projection. Its first request, a product switch, and a reconnect return an authoritative snapshot. Subsequent responses contain only changed task, chore, project, idea and runtime records. Membership/order changes carry an authoritative ID list: missing IDs are removals, including archived/deleted items and items moved to another product. Product metadata and dependency lists are replaced only when changed. Comparing full projections includes related parent/revision cards without trusting an invalidation's item IDs.

Every packet carries a session-local sequence and an explicit base sequence. The app applies it only to that exact base and product; a mismatch requests a full tree and resets the engine base. This handles duplicate/missing packets and queue shedding without treating unrelated global topic revisions as gaps. A full reply resets the client cache, so the next opted-in request establishes a fresh base. The existing selected-product apply path receives the reconstructed tree and retains its cache-eviction, selection, popover and history behavior.

Database assembly now uses one read transaction for a consistent projection. The read dispatcher can admit concurrent requests; one session lock covers snapshot acquisition, cache advancement and response enqueue so an older snapshot cannot overtake a newer one. The cache is bounded to one product per session; it is discarded with the session. Old clients omit the opt-in flag and receive the existing wire response. Worker-tier sanitization removes private idea content from both snapshots and patches.

This is a transmission optimization: the engine still assembles all board projections, and the app still applies a reconstructed tree. Initial snapshots retain complete descriptions and expandable history. Measurements distinguish that remaining assembly cost from bytes saved on refresh.

## Matched synthetic before/after

A fresh October 12 baseline used the reproduction command above on the parent revision: 20 samples, identical fixture and local macOS Bazel fastbuild configuration, no cache overrides. Baseline DB assembly plus full serialization p50/p95 was 3,903.989 / 4,266.569 ms; review-state p50/p95 was 2,184.991 / 2,438.353 ms. Host load remains uncontrolled. The earlier authorized snapshot is historical evidence only; this revision's matched comparison uses the deterministic fixture.

The initial optimized run, before the final main rebase, passed all fixture cardinality, foreign-key, review-state and delta-size assertions:

| Metric                                   |     Before |      After |
| ---------------------------------------- | ---------: | ---------: |
| DB assembly + full serialization p50, ms |  3,903.989 |  1,658.907 |
| DB assembly + full serialization p95, ms |  4,266.569 |  1,809.704 |
| Review-state p50, ms                     |  2,184.991 |    169.386 |
| Review-state p95, ms                     |  2,438.353 |    175.686 |
| Complete tree body, bytes                | 29,853,645 | 29,853,645 |
| Runtime queries per fetch                |     22,755 |     22,755 |

Review-state p50 improved by 12.9×; complete assembly plus serialization p50 improved by 2.35×. The initial snapshot retains all information and adds 77 bytes of update metadata (29,853,722 bytes total).

The same synthetic harness then performed real name mutations and ran production reconciliation. These are single measurements per batch size, not percentile estimates:

| Changed database records | Baseline full-tree body, bytes | Delta update body, bytes | DB + diff + serialization, ms |
| ------------------------ | -----------------------------: | -----------------------: | ----------------------------: |
| 1                        |                     29,853,645 |                    3,862 |                     1,298.944 |
| 10                       |                     29,853,645 |                   37,504 |                     1,296.298 |
| 100                      |                     29,853,645 |                  374,269 |                     1,295.569 |

The delta sizes include the product ID, sequence/base, changed records and collection metadata, excluding the outer RPC envelope and newline (as the full-tree body measurement does). One-record refresh transmission falls by approximately 99.987%. These measurements exclude queueing, socket delivery and app decoding. Database work remains product-wide; the unchanged runtime-query count makes that limitation explicit.

Validation covers exact snapshot equality, unchanged refreshes, product moves, deletion, full-response recovery, reconnects, unrelated global revisions, patch ordering and worker idea sanitization. The initial full-suite run exposed a benchmark-only pooled-connection deadlock: the mutation loop retained its connection guard while calling the tree reader. Releasing that guard before the read fixed the root cause; no assertion or ordinary test timeout was relaxed.

A further 20-sample run after rebasing onto current main passed with identical snapshot and delta body sizes. DB assembly plus full serialization p50/p95 was 1,852.617 / 3,440.486 ms; review-state p50/p95 was 204.258 / 466.449 ms (10.7× faster p50 than the fresh baseline). The 1 / 10 / 100-record delta probes took 1,339.380 / 1,327.944 / 1,399.412 ms. The timing variation reinforces the uncontrolled-host-load caveat; byte counts remained exact. Final integration also clears the client's base on a new decode connection generation, preventing an old session's sequence from matching a new session after a lost reconnect snapshot.

## Review projection cache

A board assembly also resolves the AI-review projection (`ai_review_state`, `ai_review_findings_revision_id`, `ai_review_badge`) for every card that can show one. The lookup set is every active, in-review _and done_ card with a PR, so on the synthetic fixture it is 7,067 of 7,585 cards, and both review queries were keyed by card: each walked the card's revision chain to its PR owner in SQL and then read the owner's `tasks` row. Those rows carry multi-kilobyte descriptions, so the pass was dominated by page reads of the `tasks` table — the `sqlite3VdbeExec` / `pread` profile of an engine that sits at a full core while the app refetches the board after every structural invalidation. The query plans themselves were already index-only (`EXPLAIN QUERY PLAN` shows no table scan; the recursive family walk uses `idx_tasks_parent_task_id` and the verdict probe uses `pr_review_verdicts_work_item_idx`); the cost was the number of rows the plans had to touch per assembly, which scaled with the board rather than with what changed.

The projection is now resolved per PR owner and cached in the engine-wide `BoardReviewCache` (`tools/boss/engine/work-db/src/board_review_cache.rs`), validated by `board_review_epoch` (schema 53). SQLite triggers bump an item's epoch, and its parent's, inside the writer's transaction whenever a projection input is written: the listed `tasks` columns, any `pr_review_verdicts` row, the kind/status/head columns of `work_executions`, and `worker_proposals`. A pass reads member IDs and epochs only for the requested owners from a constant statement: owners are the bound id list (`json_each`), children walk `idx_tasks_parent_kind_id` (schema 54, covering `parent_task_id, kind, id`), and the key also includes any findings revision a family verdict still names (read through the partial covering index `pr_review_verdicts_revision_idx`, schema 55, so a warm pass reads only verdict rows that name a revision and its cost does not grow with review history), so a reparented findings revision continues to invalidate its original owner. The projection and the key read are built from one shared family-walk fragment over `idx_tasks_parent_kind_id`; the single-column `idx_tasks_parent_task_id` it superseded is dropped in schema 55. It probes the cache by the sorted member/epoch pairs and runs the two owner-keyed queries only for the owners whose key moved. Running and queued reviews stay a per-pass read of live executions. A card whose chain leaves the board (an archived ancestor) keeps the card-keyed SQL walk. The `db.ai_review_state` population segment now records the cards resolved (`rows`) and the statements run (`db_queries`), and `work_tree_production_scale_measurement` pins those statement counts: 2 on a warm pass, 4 after any number of verdict writes, with exactly one owner recomputed per written family.

### Measurements on the synthetic fixture

The October 27 warm-cache timings below were measured on the earlier family-key read that still fetched every owner's and member's `tasks` row (the Owner CTE re-checked `kind` and the recursive step used `idx_tasks_parent_task_id`). They are not measurements of the covering-index key read. The pinned statement counts are unchanged and remain the regression guard. The covering-index run follows this table.

Measured on October 27 with the fastbuild configuration. The host was heavily loaded (other Bazel builds and the production engine), so absolute times in this run are 3–5× slower than the idle baseline below and only the interleaved comparisons within one run are meaningful. `measure_cached_reviews` alternates an uncached and a warm cached assembly for each sample, so the two columns of each row saw the same load.

| `db.ai_review_state` per assembly     | Uncached (owner-keyed, every owner) | Warm cache (earlier key shape) |
| ------------------------------------- | ----------------------------------: | -----------------------------: |
| p50 over 5 interleaved samples, ms    |                               1,095 |                            373 |
| After 1 / 10 / 100 verdict writes, ms |               1,184 / 1,438 / 1,140 |                 352 / 44 / 119 |
| Review statements                     |                                   3 |             2 (4 after writes) |
| Owners recomputed                     |                         5,301 (all) |  0 (1 / 10 / 100 after writes) |

The unmodified engine resolved the same fixture in 222 ms per assembly on an idle host (one sample from the baseline run; whole warm assembly 567 ms), reading 7,067 owner rows by card. The pinned statement counts are the regression guard; the timings show the direction and the write-proportional recompute, not a stable absolute.

Covering-index key read, measured October 27 on the same fixture and `measure_cached_reviews` harness (5 interleaved samples, fastbuild, loaded host). Statement counts and owners-recomputed are unchanged.

| `db.ai_review_state` per assembly     | Uncached (owner-keyed, every owner) | Warm cache (covering-index key) |
| ------------------------------------- | ----------------------------------: | ------------------------------: |
| p50 over 5 interleaved samples, ms    |                                 848 |                             414 |
| After 1 / 10 / 100 verdict writes, ms |                     510 / 527 / 806 |                  91 / 365 / 345 |
| Review statements                     |                                   3 |              2 (4 after writes) |
| Owners recomputed                     |                         5,301 (all) |   0 (1 / 10 / 100 after writes) |

The board's guide state (`db.review_guide_state`) resolves with one constant statement that binds the board's root ids as a JSON array and probes `pr_review_guide_source_series_observation_idx` once per root; it never reads `tasks` (pinned by an `EXPLAIN QUERY PLAN` test), so its cost follows the board rather than the product's task history. One run of `work_tree_production_scale_measurement` on the same fixture (fastbuild, loaded host) recorded 25.5 ms for the segment over 7,585 cards. The same single-sample run recorded the warm review pass at 71 ms (`db.ai_review_state`) against 129 ms uncached, with 2 statements, using the partial verdict index (earlier covering-index run: p50 414 ms assembly over 5 samples; this is one sample on a differently loaded host, so it shows direction only). The key read's VM-step count is pinned independent of verdict history by `warm_key_read_work_does_not_grow_with_verdict_history`.

### Isolated engine, before and after

Two engine binaries — the unmodified `main` build and this change — were started as test fixtures (`--socket-path /tmp/boss-test-<uuid>.sock`, `BOSS_WORKER_POOL_SIZE=0`, no events socket) on fresh copies of the same seeded fixture database (schema 52; the changed engine migrated it to 53 at startup). A driver fetched the board once per second over the control socket with `delta: true`, writing one runtime-irrelevant `work_runs` column between fetches so every fetch assembled the tree, and `sample <pid> 5` was taken under that load. Runs alternated old/new/old/new on a heavily loaded host, so wall-clock latency varied several-fold between runs; engine CPU seconds per fetch is the comparable figure.

| Run           | Fetches in 60 s | Engine CPU, s | CPU per fetch, s | Fetch latency p50, s |
| ------------- | --------------: | ------------: | ---------------: | -------------------: |
| Before, run 1 |              19 |         15.93 |             0.84 |                 2.65 |
| After, run 1  |               9 |          6.55 |             0.73 |                 7.12 |
| Before, run 2 |               8 |          8.34 |             1.04 |                 6.62 |
| After, run 2  |              16 |         11.10 |             0.69 |                 3.18 |

Idle (no fetches) both engines sat at 0.3–2.7% of a core. The fixture's 24 MB of descriptions make tree serialization and delta diffing the bulk of a fetch here, which caps the per-fetch gain this harness can show; the sample attribution is the sharper signal. Attributing every `sqlite3_step` sample to its nearest engine caller, as in the original report:

| Nearest engine caller under `sqlite3_step`         | Before | After (badge cache) | After (badge cache + guide scan) |
| -------------------------------------------------- | -----: | ------------------: | -------------------------------: |
| `review_badge` (current-head state, badge history) |    932 |                   0 |                                0 |
| `query_review_guide_card_states`                   |     95 |                 630 |                                0 |
| `collect_rows` (the tasks lane read)               |     29 |                 319 |                                0 |
| Total `sqlite3_step` samples in 5 s                |  1,056 |                 996 |                                4 |

With badge resolution cached, the review-guide lookup became the largest remaining SQLite cost: it bound every PR-bearing card as a `(root_task_id, canonical_pr_url)` row-value `IN` list, which SQLite answers by building a two-column ephemeral index of the whole board on every assembly (`balance_nonroot` dominated its stack). A board-sized root set now joins `pr_review_guide_source_series` to `tasks` on `product_id` (fixed SQL, statement-cached) and keeps matching roots in memory by current PR URL. Single-item projections and other chains at or under the ancestry depth cap keep the indexed `root_task_id IN (...)` list. Unrelated products are not scanned. The guide-scan column above records the earlier full-table scan, not the product join. `work_tree_production_scale_measurement` exercises the product-scoped board path as part of each full assembly; `db.review_guide_state` on that fixture is the sample of this lookup.

### Single-item diagnostics

Every `attach_task_derived_projections` invocation emits `single_item.total` duration to the existing engine population timing log. The request ID is `single-item:{invocation}:{caller}`, where `caller` is the trigger label already known to the mutation path (`update_task`, `set_task_doc`, `answer_operator_question`, …) or `get_work_item` on the read path. Group by that label for attribution; it is not a Rust source location, which collapsed every `WorkItemUpdated` derivation onto the wrapper. `items` records the loaded chain size; `single_item.ai_review_state` records review duration, eligible cards (`rows`), and executed review statements (`db_queries`). Count total records per interval for frequency. Failed derivations still emit total duration, while a failed review stage emits duration without a misleading successful query count.
