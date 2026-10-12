# Work-tree scale measurement

This PR implements session-local work-tree deltas and pins the recursive review-family lookup to the existing parent index. Both take effect when the updated engine and app are deployed; older clients continue receiving full trees. Invalidation diagnostics take effect on engine deploy; the benchmark runs only under Bazel. No production database, identifiers, descriptions, or other row contents belong in this report or its fixture.

## Reproduction

Run the deterministic fixture through the real database assembly and serialization path:

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_filter=work_tree_production_scale_measurement --test_output=all --test_arg=--nocapture --test_timeout=1800 --test_env=BOSS_BENCH_SAMPLES=20
```

For an explicitly authorized consistent SQLite snapshot, change the filter to `work_tree_authorized_snapshot_measurement` and add `--test_env=BOSS_BENCH_SNAPSHOT=/absolute/path/to/snapshot.db`. The measurement opens that source with `immutable=1` and read-only flags, copies it into the test's private temporary directory with `VACUUM INTO`, and initializes only the copy. Tests never contact the production engine. The optional snapshot test emits aggregate counts, sizes, and durations only.

The synthetic fixture has 8,030 tasks (6,640 done, 445 archived, 350 in review, 321 todo, 197 blocked, 77 active), 131 projects, five products, 21,127 executions, 19,703 runs, 4,131 proposals, 786 review batches, and 1,818 verdicts. It checks every table count and foreign-key integrity. Assumptions: one large product, four empty products, fixed 3 KiB briefs, one revision per four tasks, completed review executions, and clean verdicts. It deliberately makes no claim to reproduce production's revision topology, description distribution, or event arrival process.

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

Database assembly now uses one read transaction for a consistent projection. The request dispatcher is sequential per connection, and cache advancement plus response enqueue occur under one lock. The cache is bounded to one product per session; it is discarded with the session. Old clients omit the opt-in flag and receive the existing wire response. Worker-tier sanitization removes private idea content from both snapshots and patches.

This is a transmission optimization: the engine still assembles all board projections, and the app still applies a reconstructed tree. Initial snapshots retain complete descriptions and expandable history. Measurements distinguish that remaining assembly cost from bytes saved on refresh.

## Matched synthetic before/after

A fresh October 12 baseline used the reproduction command above on the parent revision: 20 samples, identical fixture and local macOS Bazel fastbuild configuration, no cache overrides. Baseline DB assembly plus full serialization p50/p95 was 3,903.989 / 4,266.569 ms; review-state p50/p95 was 2,184.991 / 2,438.353 ms. Host load remains uncontrolled. The earlier authorized snapshot is historical evidence only; this revision's matched comparison uses the deterministic fixture.

The optimized run passed all fixture cardinality, foreign-key, review-state and delta-size assertions:

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
