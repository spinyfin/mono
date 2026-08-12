# Decomposing `engine_lib`: where the crate boundaries actually are

`//tools/boss/engine/core:engine_lib` is one Rust rlib built from 461 files
and ~318k lines. Every edit under `tools/boss/engine/core/src/` recompiles
all of it, so the edit-to-signal loop is flat in the size of the diff. This
investigation surveys the module graph to find where that crate can be split,
records the measurements, and names the seams that were rejected.

The headline result is that **the obvious seams do not exist yet**: 84% of the
crate is one mutually-recursive component, and the one large defensible
boundary (`work`, the SQLite persistence layer) is reachable only after
cutting a specific set of upward edges. The change that lands alongside this
document cuts five of them, removing three modules (`ci_watch`,
`design_detector`, `merge_poller`) from `work`'s dependency set entirely and
taking `work` from 19 outbound code edges to 16. The rest are enumerated below
with the design decision each needs.

**No build-time improvement is claimed for that change, and none was
measured** — it cuts coupling, it does not yet move any code across a crate
boundary. The win arrives only when `work` actually leaves `engine_lib`.

## Measurements

All numbers are warm-cache incremental rebuilds on an Apple-silicon dev box:
append one comment line to `src/codex_guard_trace.rs`, rebuild, repeat.

| target            | steady-state warm incremental |
| ----------------- | ----------------------------- |
| `engine_lib`      | 21.9 – 22.9 s                 |
| `engine_lib_test` | 42.1 – 44.7 s                 |

Two calibration notes for anyone repeating this:

- **The first measurement after a cold or partially-populated disk cache is
  not the steady state.** An initial reading of 30.7 s / 65.8 s dropped to
  ~22 s / ~43 s once the cache was warm, with no source change responsible.
  A/B the tree (revert, measure, restore, measure) rather than comparing
  against a number captured earlier in the session.
- `engine_lib_test`'s 11-way sharding is already optimal
  (`max = 26.9s, min = 26.0s, dev = 0.3s`). Execution is not the problem; the
  ~43 s is a single `cfg(test)` compile of the whole crate. See
  [`rust-test-sharding-engine-lib-test.md`](rust-test-sharding-engine-lib-test.md).

## Why splitting helps, and when it does not

A crate split only pays off in one direction. For a chain `low -> mid -> high`:

- editing `low` rebuilds all three — no better than today;
- editing `high` rebuilds only `high` — the full win.

So extracting a leaf that the core still depends on does **not** speed up
edits to that leaf: the core rebuild dominates either way. The lever that
works is making the core itself smaller, by moving large cohesive chunks
_below_ it. Everything that follows is scoped by that constraint.

## The module graph

157 top-level modules, 462 files, 318,378 lines (153k production, 165k test).
Edges were extracted by parsing `crate::<module>` references with doc comments
stripped — intra-doc links (`[crate::foo::Bar]`) massively overstate real
coupling and must be excluded. `work` appears to depend on 56 sibling modules;
only 19 of those are code.

**84% of the crate is a single strongly-connected component**: 88 modules,
336 files, 266,193 lines are mutually recursive. No crate boundary can pass
through that component without first breaking cycles.

The cycles are not caused by one bad edge. Treating the largest single
offender (`attention_lifecycle`, below) as a leaf shrinks the component from
88 modules to 87 — the density is real, not an artefact of one registry.

Outside the component:

- a **bottom layer** of 59 modules / 75 files / 28,330 lines that depend on
  nothing in-crate, but which is fragmented into unrelated single-file
  modules — bundling it would produce exactly the catch-all crate that is
  not wanted;
- a **top layer** of 14 modules / 71 files / 27,612 lines that nothing
  depends on, which is almost entirely `#[cfg(test)]` code
  (`coordinator_tests`, `conflict_watch_tests`, `ci_watch_tests`,
  `worker_setup_tests`, `conformance`). These cost nothing in `engine_lib`
  and cannot move out of `engine_lib_test`: they exercise `pub(crate)`
  internals, so they cannot become integration tests without widening a
  large amount of the crate's surface.

## The chosen seam: `work`

`work` is the SQLite persistence layer: 106 files, 81,451 lines — 26% of the
crate. It is the one large chunk that is genuinely cohesive, sits at the
bottom of the intended layering, and has thin upward coupling: 19 outbound
code edges against 75 modules that depend on it.

Extracting it yields `boss-engine-work` at **117 files / 85,411 lines
(26.8%)**, leaving 345 files / 232,967 lines in the core. Alongside `work`
itself, eleven small satellites must move down, because they are types and
helpers `work` persists rather than independent subsystems:

`work_dependencies`, `event_publish`, `run_cost`, `cost_report`,
`cost_pricing`, `audit_effort`, `worker_escalation`, `deferred_scope`,
`merge_mechanism`, `population_timing`, `reconcile_audit`.

Projected effect, extrapolating from the ~0.20 s per 1,000 lines the current
compile costs: `engine_lib` ~22 s → ~16 s, `engine_lib_test` ~43 s → ~32 s,
plus `work` gains its own test target so 48 test files / 35k lines of test
code stop rebuilding on every unrelated core edit. **These are projections,
not measurements — the split is not yet done.**

### The eight upward edges

`work` reaches _up_ out of the persistence layer in eight places. Each has to
be resolved before the crate can move.

| edge                  | what `work` needs                                                                                                    | status                                                                      |
| --------------------- | -------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `merge_poller`        | `parse_pr_number`                                                                                                    | **cut** — now calls `boss_github::pr_url::pr_number_from_url` directly      |
| `design_detector`     | `task_uses_per_task_doc`                                                                                             | **cut** — predicate moved to `work/products_design.rs`                      |
| `ci_watch`            | `merge_queue_rebounce_pr_head`                                                                                       | **cut** — moved to `work/insert_helpers.rs`, beside the rows it keys        |
| `completion`          | `parse_repo_slug`                                                                                                    | **cut** — now calls `git_utils::repo_slug::parse_github_slug`               |
| `completion`          | `should_enqueue_reviewer_for_primary`                                                                                | **cut** — moved to `work/exec_status_helpers.rs`, beside its other consumer |
| `completion`          | `expected_branch_name`, `branches_identify_same_work_item`                                                           | open                                                                        |
| `coordinator`         | `pool_dispatch_policy_for_worker_id`, `pool_driver_slug_for_execution_kind`, `kind_always_dispatches_on_pool_driver` | open                                                                        |
| `host_registry`       | `ensure_local_host`, `refresh_local_host_auto_capabilities`, six `migrate_*` functions                               | open                                                                        |
| `pr_url_capture`      | `validate_pr_url`                                                                                                    | open                                                                        |
| `attention_lifecycle` | `lifecycle_for`, `AttentionLifecycle`, `ClearedBy`, `automatically_cleared`, two kind constants                      | open — see below                                                            |

(Plus `test_support`, a test-only helper that moves or is duplicated with the
test files.)

### `attention_lifecycle` is the hard one

`ATTENTION_LIFECYCLES` is a central registry mapping each attention kind to
what clears it. It is declared in `attention_lifecycle` but its rows _name
constants declared in 22 other modules_ — including `crate::work::*` at the
bottom of the graph and `crate::app::readoption::*` at the top. `work` in turn
calls `lifecycle_for` from `attention_filing` and consumes the lifecycle types
in `attention_reconcile`.

That single table welds the bottom and top of the engine into one cycle.
Two viable resolutions, neither free:

1. **Move the kind constants and their lifecycle rows down** into a
   `boss-engine-attention-kinds` crate that both `work` and the 22 producers
   depend on. Cohesive and already a documented domain concept
   (`tools/boss/docs/attention-lifecycle.md`), but it touches 22 modules.
2. **Invert the two consumers.** `warn_if_lifecycle_undeclared` is
   warning-only and could take an injected predicate;
   `reconcile_stale_attention_signals` could receive `&[AttentionLifecycle]`
   from its caller instead of reaching for the global table. Cheaper, and
   arguably better design — the persistence layer stops owning a policy
   registry — but it changes two signatures.

Option 2 is the recommendation: it is smaller, and it leaves the registry
where producers already maintain it.

## Seams that were rejected

- **The Codex driver surface** (`codex_guard_trace`,
  `codex_unobserved_command`, `codex_home_retention_sweep`, and siblings).
  This was the suggested starting point and has real edit demand, but it is
  **2,190 lines — 0.7% of the crate**, and `codex_guard_trace` is inside the
  strongly-connected component. Extracting it would not move the measurement.
- **`app`** (75 files, 46k lines) has the cleanest boundary in the whole
  graph — only 7 inbound edges. It was rejected because it sits at the _top_:
  extracting it leaves every core edit rebuilding core **and** `app`
  sequentially, which is slower than today. It becomes attractive only for
  workers editing `app` itself, after the core has already shrunk.
- **`completion`** (26 files, 26k lines): 29 outbound edges, deep in the
  component. Strictly worse than `work`.
- **The bottom layer as one crate** (59 modules, 28k lines): this is the
  forbidden catch-all. The modules share no vocabulary; the crate would exist
  only to hold "things with no dependencies".
- **Sub-splitting `work`**: the 16 files carrying upward edges are spread
  across the module rather than concentrated, so there is no clean internal
  seam to extract a subset.

## Recommended order

1. Cut the five remaining upward edges (this document's table), smallest
   first; `attention_lifecycle` last, via inversion.
2. Move `work` plus its eleven satellites to `//tools/boss/engine/work`.
   Follow the crate's existing facade convention — `pub use boss_engine_work
as work;` in `lib.rs`, matching `boss_metrics`, `boss_engine_driver`, and
   the dozen other modules already aliased that way — so the ~600
   `crate::work::…` call sites are untouched.
3. Expect ~50 `pub(crate)` → `pub` widenings; the compiler enumerates them.
   Grant the narrowest visibility each edge needs.
4. Give `work` its own `rust_test`, moving its 48 test files with it.
5. Re-measure with an A/B revert, not against a remembered number.
