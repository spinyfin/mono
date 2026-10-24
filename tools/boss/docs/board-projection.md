# Incremental hub board projection

The hub retains a revisioned board per recently read product. A task status change, execution transition, run-state change, review verdict, proposal, question or attachment refreshes its affected cards and their derived fields. This implements the board track in the [hub-and-spoke design](designs/hub-and-spoke-distributed-engine.md), using the existing execution/run inputs before local spoke adoption (§2.4 of the investigation).

## Input and recovery contract

Schema 56 adds a transactionally ordered `board_changes` journal. The runtime input source is schema 49's `board_runtime_epoch`, which already covers execution and run writes from every connection. Card changes also record old and new parent links. These durable inputs establish completeness independently of asynchronous invalidation publication and frontend coalescing. A transport-specific observation must first enter the hub's authoritative runtime projection; the pure `boss-board-projection` crate accepts typed cards and runtime values and knows nothing about SQL or worker transport.

Each read opens one deferred transaction, reads the journal watermark, expands the union of old and current relationships, and reads the complete affected cards through the same derivations as full assembly. The union includes ancestors, descendants and the family a reparented/deleted item left. Pending runtimes are reevaluated because dependency and chain-hold decisions can consult another product; deadlines are evaluated at the read's time even without a write. Dependencies are refreshed to preserve membership changes. Task and chore order use the item ID as a deterministic final tie break, so partial reads and full reads agree even when timestamps and ordinals tie.

The model applies a batch only against its exact base revision. The database journal revision is internal; the frontend retains its existing work revision and per-session delta sequence. Disconnects, product switches and a missing client delta base still recover through the existing full-snapshot protocol. Hub restart or cache eviction reconstructs from the database.

Bounds are explicit:

- The journal retains 4,096 records, pruned in the writing transaction. A reader behind that window performs one complete snapshot.
- Relationship closure and pending-runtime refresh are limited to 256 IDs. Both the recursive SQL and the in-memory expansion stop at that bound. An oversized batch performs one complete snapshot, with no retry loop or partial apply.
- At most four products are retained, each with at most 16,384 cards and 32 MiB of encoded content. After a snapshot, a conservative byte total is adjusted from the replaced cards, runtimes and dependency list; the retention guard does not serialize the whole board on each update. Parity sequences verify this total against actual encoding (at most six bytes of comma-count slack). Oversized boards are returned intact through full reads and are not retained. These are retention bounds, not a claim that Rust heap usage equals encoded size. In-flight responses and existing per-session wire caches have their own lifetimes.
- Changes to product/project metadata, ideas, dependencies, product alerts, planner summaries, review-batch membership and review-guide state recover from a complete snapshot. They can affect non-card collections or relationships beyond an individual family. Only the review-verdict watermark is observed in the engine metadata table; unrelated scheduler metadata does not force a snapshot.

New projection inputs must be added to the journal coverage or the full-snapshot class. The input table list lives beside the migration in `work/board_projection_schema.rs`. No call site may substitute a best-effort invalidation ID for that coverage.

## Validation and measurement

`board_projection_tests` compares every serialized field against the uncached full projection after runtime and structural sequences, including cross-product dependencies, chain holds, deadlines, nested revisions, reparenting, deletion, product/lane moves, review evidence, external commits and rollback. It verifies journal overflow, affected-set recovery, byte accounting and eviction without response truncation as well. The standalone model tests reject a missing revision base without mutating state.

The scale test reuses the existing 8,030-task / 21,127-execution fixture and changes 8, 32 and 64 worker cards and their live execution states per batch. It checks parity against both cached and uncached complete assembly. CPU is process user plus system time; wall time includes scheduling contention. Transaction writes, including journal maintenance, are measured separately. The retained encoded-size bound is a memory proxy, while process peak RSS includes the test, both comparison paths and wire serialization. Wire bytes come from the real `WorkTreeCache` delta encoder. Materializing the response and comparing complete projections still depend on board size; wire encoding cost is reported separately from incremental maintenance.

```sh
bazel test //tools/boss/engine/core:engine_lib_test --test_filter=board_projection_scale_measurement \
  --test_sharding_strategy=disabled --test_output=all --test_arg=--nocapture \
  --test_timeout=2400 --test_env=BOSS_BENCH_SAMPLES=5
```

Live counters distinguish `board.projection.incremental`, `board.projection.cards` and `board.projection.snapshot.<reason>`. The existing `board.assembly.full` counter counts actual complete assemblies, including recovery. The measurements establish per-batch costs on a synthetic board, not a concurrent-worker capacity limit or a production invalidation mix.

### Observed local results

Bazel fastbuild on macOS arm64, five samples at each point, with all 15 projections equal to both complete-assembly paths. The table reports medians in milliseconds; the write column includes the journal triggers. CPU reduction compares refresh CPU only.

| Changed cards | Incremental CPU | Cached full CPU | CPU reduction | Write CPU | Delta bytes (range) |
| ------------- | --------------: | --------------: | ------------: | --------: | ------------------: |
| 8             |            37.2 |           537.5 |         93.1% |       2.5 |       31,847–33,393 |
| 32            |            57.6 |           569.8 |         89.9% |       7.9 |     127,029–133,213 |
| 64            |            62.4 |           575.0 |         89.1% |      18.9 |     253,941–266,309 |

Median incremental/full wall times were 265/4,985 ms, 601/2,460 ms and 361/1,806 ms respectively. Median complete-tree diff/encoding wall times were 4,332, 2,813 and 1,441 ms. Wall times were much larger than CPU time and non-monotonic with batch size, so these measurements do not establish a capacity limit.

Full snapshots were 30,022,430–30,029,596 bytes; each observed delta was less than 1% of its snapshot. This uses the existing delta encoder and is not a new wire format. The maximum retained encoded-size bound was 30,036,698 bytes (28.65 MiB) for the product. Process peak RSS reached 553,009,152 bytes (527.4 MiB), including fixture creation, parity validation and both complete comparison paths; it is not the projection's heap size.
