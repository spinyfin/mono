# Work-tree scale measurement

This is the measurement stage of the scoped board-update work. It does not change board synchronization or review queries. Invalidation diagnostics take effect on engine deploy; the benchmark runs only under Bazel. No production database, identifiers, descriptions, or other row contents belong in this report or its fixture.

## Reproduction

Run the deterministic fixture through the real database assembly and serialization path:

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_filter=work_tree_production_scale_measurement --test_output=all --test_arg=--nocapture --test_timeout=1800 --test_env=BOSS_BENCH_SAMPLES=20
```

For an explicitly authorized consistent SQLite snapshot, change the filter to `work_tree_authorized_snapshot_measurement` and add `--test_env=BOSS_BENCH_SNAPSHOT=/absolute/path/to/snapshot.db`. The measurement opens that source with `immutable=1` and read-only flags, copies it into the test's private temporary directory with `VACUUM INTO`, and initializes only the copy. Tests never contact the production engine. The optional snapshot test emits aggregate counts, sizes, and durations only.

The synthetic fixture has 8,030 tasks (6,640 done, 445 archived, 350 in review, 321 todo, 197 blocked, 77 active), 131 projects, five products, 21,127 executions, 19,703 runs, 4,131 proposals, 786 review batches, and 1,818 verdicts. It checks every table count and foreign-key integrity. Assumptions: one large product, four empty products, fixed 3 KiB briefs, one revision per four tasks, completed review executions, and clean verdicts. It deliberately makes no claim to reproduce production's revision topology, description distribution, or event arrival process.

Ordinary regression runs fetch once; the benchmark command requests twenty sequential samples including the first fetch. Percentiles use nearest rank. `fetch_db_and_serialize_ms` measures `get_work_tree_instrumented` plus JSON serialization; it excludes request queuing, Unix-socket transport, and app decoding. `ai_review_state_ms` uses the existing engine population trace. Raw record sizes are sizing experiments, not an implemented delta protocol: they exclude envelope, dependencies, runtimes, related-card expansion, and removals.

## Baseline measurements

Measured on October 11 using the local macOS Bazel fastbuild configuration. Each fixture ran separately, with twenty sequential samples and no cache overrides. Host load was uncontrolled; these are baseline measurements, not a comparison with the production engine's build or load. No optimized implementation ships in this measurement stage, so there is no engine before/after comparison yet.

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

A non-coalesced selected-product invalidation therefore causes a tree body of approximately 26 MB on the snapshot, even when it carries one item ID. The table excludes protocol-envelope bytes and all queue/socket time. The first-N record sizes are illustrative slices in query order, not typical patch sizes or an implemented delta path. Descriptions account for approximately 75% of the snapshot's tree body.

The initial concurrent two-fixture benchmark exceeded its 600-second window on the snapshot (the synthetic test passed). Running the measurements independently completed in 158 seconds for the synthetic fixture and 407 seconds for the snapshot. The reproduction command allows an explicit 1,800-second window for twenty slow baseline fetches; the repository's ordinary test timeout is unchanged, and its one-sample regression passed in the complete test suite. All fixture integrity and review-state assertions remain enabled in both modes.

## Refetch sources

Source inspection finds 32 mutation callers of `publish_work_invalidation`, plus batch mutation and execution publishers. The selected-product app handler ignores `item_ids` for board refresh and calls `refetchForInvalidation` for either a matching product topic or matching payload product. That schedules a complete tree and requests attention/planner data. Existing 150 ms coalescing is not a correctness or bandwidth solution.

| Source                                                                                          | Items carried             | Current board consequence                  |
| ----------------------------------------------------------------------------------------------- | ------------------------- | ------------------------------------------ |
| Execution publisher: lifecycle, review, PR state changes                                        | One                       | Entire selected product                    |
| Work-item CRUD, document pointer, product defaults, external tracker, automation-created task   | Usually one               | Entire selected product                    |
| Dependency add/remove                                                                           | Two endpoints             | Entire selected product                    |
| Project postmortem                                                                              | Project and task          | Entire selected product                    |
| Project reorder, planner release/unpopulate, operator answer, idea graduation, attention action | Variable affected-ID list | Entire selected product                    |
| Batch work mutation                                                                             | IDs grouped by product    | Entire selected product                    |
| Comment artifact invalidation                                                                   | One artifact; no product  | Open comment layer; no direct tree refresh |

Additional full-tree paths include CRUD result handlers, planner completion, `products_list` (including a second cold-start fetch), reconnect/resync, product selection, and manual refresh. Fixing only the `workInvalidated` arm would leave other amplification paths.

New `work invalidation fanout` records report source, reason, product correlation, item count, and requested topic count (before de-duplication) at the mutation, batch, and execution publishers without logging item contents. Group these by source/reason to obtain arrival counts; join with the existing population-timing and frontend-frame diagnostics to measure actual refetches and bytes. Publisher counts are not equivalent to app refetch counts because subscriptions and existing coalescing intervene. Historical event frequency cannot be recovered from a state-only SQLite snapshot. The coordinator's supplied observations remain the historical evidence: 19–58 tree refetches per ten minutes and five disconnections in approximately eleven minutes.

## Visibility and payload

The authorized October 11 snapshot contains 8,108 task rows: 6,702 done, 461 archived, 334 in review, 333 todo, 201 blocked, and 77 active. Across all products, done descriptions occupy 26,125,681 characters. These are newer counts than the October 9 fixture specification.

For the largest product, non-deleted rows include 4,938 done and 434 open tasks; 30 archived rows are already excluded by the tree queries. Of the non-archived rows, 2,395 are terminal revisions hidden as standalone board cards (their summaries appear on parent cards). They account for 44.6% of the 5,372 non-archived rows, leaving at most 2,977 standalone cards before project and view filters. Their descriptions alone occupy 9,871,883 characters. This is an upper bound on removable detail, not permission to discard revision projections.

The Done lane groups history into collapsible sections and still permits expanding Earlier. It has no history cutoff. Preserve this behavior: introduce detail/history loading before removing information. `WorkCardPopover` also reads descriptions from the current task object, so clearing descriptions without adding detail loading would silently break that view.

## Query-plan diagnosis

A read-only SQL experiment over 5,054 PR-bearing/revision IDs on the authorized snapshot found the recursive family step using `SEARCH t USING AUTOMATIC PARTIAL COVERING INDEX (kind=?)`, rather than the existing `idx_tasks_parent_task_id`. The same plan appears after excluding design, postmortem, and investigation kinds to match the engine review selector (4,914 IDs). This repeatedly visits revision candidates for each family node. The correlated verdict lookup additionally sorts candidates; the completed-execution lookup chooses `work_executions_ready_idx (status=?)` and sorts.

The timing experiment used the broader 5,054-ID diagnostic input. Three standalone Python/SQLite baseline executions of the extracted current-head query took 7,276 / 15,812 / 30,341 ms. A diagnostic query forcing the existing parent index took 1,693 / 688 / 1,332 ms. These are noisy, non-interleaved query experiments during local compilation, not engine before/after benchmarks and not production performance promises. No query change ships here. A fix should use the engine's SQLite build, verify identical verdicts and cycle behavior, and pin the relevant query plan with a regression test rather than adding an unproven index.

## Proposed split

The measured waste is not just a missing point lookup: thousands of hidden revision rows feed parent projections, all historical Done rows remain expandable, and the existing revision counter is global across subscribed and unsubscribed topics. An item-only patch with global `last + 1` checking would either leave related cards stale or repeatedly resync on unrelated events.

Use three reviewable implementation changes after this measurement:

1. Query optimization with synthetic/authorized-copy before-and-after measurements and verdict equivalence/plan tests (approximately 150–300 changed lines).
2. Engine-owned scoped reconciliation, product-scoped sequence/replay or explicit gap response, consistent snapshot watermark, relationship closure, tombstones and product moves; thin-client patch application; reconnect/race/gap tests (approximately 1,000–1,400 changed lines).
3. Compact board records plus on-demand descriptions and expandable historical detail, with popover/search/history regression coverage (approximately 500–800 changed lines).

The combined estimate is 1,650–2,500 lines before this fixture and diagnostics, above the brief's roughly 1,500-line reviewability boundary. These are engineering estimates from the concrete paths above, not measured implementation line counts. This PR therefore uses the explicitly permitted measurement-first split. Scoped updates, payload shrinking, and the query fix remain undelivered; the board continues its current full-refetch behavior.
