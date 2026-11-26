# Boss: Token Usage window — the app never adds two token counts together

- **Date:** 2026-10-28
- **Status:** Design. Both engine requests (`GetTokenUsageRows` and `GetTokenUsageSeries`), the `boss-usage-report` crate, projections, `boss cost rows` / `boss cost series` callers, the seed fixture, and the app Token Usage table window are implemented; the over-time tab is not.
- **Revised:** 2026-11-23, against `spinyfin/mono` `main` after #3799 (Grok and Antigravity token capture). Grok and Antigravity now record tokens as **net** input, so the earlier statements that Grok records nothing and that Grok input is gross are corrected throughout; `work_runs.usage_unavailable` is the per-run capture status this design reserved; the driver, `usage_unavailable` and projection paths moved (`engine/work-db`, `engine/app`). Where this revision and an older sentence disagree, this revision wins.
- **Product:** Boss. **Project:** Token Usage window (`proj_18e2c00c59cb8790_22c`, slug `token-usage-window`).
- **Deliverable:** this document. The task breakdown at the end is the handoff to scheduling.
- **Evidence base:** every code claim below was checked against `spinyfin/mono` `main` @ `1e8e8c131daa` on 2026-10-28 and the corrections re-checked on 2026-11-23. Paths are under `tools/boss/`. Database-population figures (run counts, model strings) are carried from the cost-attribution design's 2026-07-29 brief and labelled as such; this pass had no access to the production database and did not re-measure them.
- **Related:** [`trustworthy-per-work-item-cost-attribution.md`](trustworthy-per-work-item-cost-attribution.md) (direct/inclusive, spend classes, coverage, presentation invariants), [`performance-metrics-timeseries-capture-and-interactive-swiftui-charts.md`](performance-metrics-timeseries-capture-and-interactive-swiftui-charts.md) (the generic `GetMetricSeries` contract this design does not wait for), [`engine-app-rpc.md`](engine-app-rpc.md) (socket transport and request-id correlation), `app-macos/AGENTS.md` (no modal reports).

## Verdict

Build the window as a thin client over two new read-only engine requests: a **run-grain usage-rows request** for a time window that returns the task → execution → run tree with every figure already computed (direct, inclusive, per-group, coverage, caveats), and a **usage-series request** that returns bucketed totals together with the normalizers (tokens per run, per completed execution, per agent-active hour, per completed task, per distinct PR) as engine-computed ratios. The contested property is in the title: **the app never adds, divides or compares two token counts**. It pivots, sorts and draws what the engine sends, and every regroup, filter change or range change is one round trip. A reviewer could reasonably prefer fetching rows once and aggregating in Swift for instant interactivity; the reason not to is that every honesty rule this window must honour (NULL is never zero, direct-only list totals, inclusive only on a chain root, gross input never ranked against net) would then exist twice, once in Rust and once in Swift, and drift silently. The round trip is cheap; the drift is not.

The decided three-PR shape holds with one refinement: the over-time work splits into an engine PR (series request) and an app PR (charts), because the two are different languages, different reviewers, and the engine half can be built in parallel with the table window. Four in-scope entries, one deferred.

## Goals

- A **Window → Token Usage** window in the macOS app that shows token consumption per task and drills down task → execution → run, with the driver and model each execution and run used.
- Filtering, sorting and grouping by product, project, task kind, execution kind, driver, model, effort, reasoning, status and time range, including grouping a task with its revisions: a revision chain rolled up under its root with a per-revision breakdown.
- A usage-over-time view normalized for bursty activity, so the operator can tell whether an uptick in consumption comes from more work, more expensive work, or a shift in model or driver mix.
- Absorb the deferred "macOS app cost surface" from the cost-attribution design: per-work-item cost, coverage and confidence in the app, under the same presentation invariants as the `boss cost` CLI. Every known data-quality gap is shown, never hidden and never rendered as zero.
- Engine owns data and aggregation; the app is a thin client. New capture-quality signals from the cost-attribution project (per-run capture status, per-model usage rows) must flow into the window without UI rework.

## Non-goals

- **Fixing capture defects.** Codex gross input, excluded Claude subagent tokens, `<synthetic>` labels and the uncited pricing table are the cost-attribution project's work (Grok and Antigravity capture landed separately in #3799 and is no longer a gap, though neither reports cache creation). This window _displays_ those gaps; it does not close them. Where this design needs a value those defects corrupt, it labels the figure rather than correcting it.
- **Depending on `GetMetricSeries`.** The performance-metrics design's foundation PR (#2954) is open and stalled as of 2026-10-11. This design builds its own series request and records how it folds into `GetMetricSeries` later (see "Folding into the metrics series contract").
- **Brush-to-zoom, pinch, mini-map and zoom history.** Those are specified and scheduled by the performance-metrics design for the Metrics window. The over-time view here ships range presets, a custom range and hover; it does not duplicate that gesture work.
- **Budget enforcement, alerts or dispatch gating on cost.** A read surface only.
- **A USD figure that is anything other than a labelled estimate.** No rate values appear in this document; pricing stays behind the existing `cost_pricing` seam and the cost-attribution design's replacement of it.
- **Changing the retention policy.** The 14-day prune of failed, abandoned, orphaned and cancelled executions bounds what older windows can show; this design surfaces that bound as a coverage note and leaves the policy to the performance-metrics design's retention entry.
- **Backfilling pre-instrumentation runs.** Permanently impossible per the cost-attribution design; those rows show "not instrumented".

## Background: what exists, verified against the code

### The cost read surface

`boss cost task|window|top` (#2596) is the only token read surface. It sends `GetWorkItemCostReport`, `GetCostWindowReport` and `GetTopCostConsumers` (`protocol/src/wire.rs`), whose handlers in `engine/app/src/app/cost.rs` fetch flat `CostRunRecord` rows from `engine/work-db/src/cost_report_db.rs` and fold them through pure builders in `engine/core/src/cost_report.rs`. The builders uphold the NULL-versus-zero contract documented on `CostMeasurement` (`protocol/src/types/cost_report.rs`): a run with no recorded token class is `runs_unmeasured` and excluded from every sum; a measured run missing a class leaves that class unknown and sets `tokens_partial`; a recorded zero is `runs_zero`. `TOKEN_CAPTURE_START_EPOCH_S` (`cost_report.rs`, 2026-07-27T09:23Z) is echoed back so a client can flag a window spanning the capture boundary. Pricing (`engine/core/src/cost_pricing.rs`) is a three-family substring table with no provenance, applied only on request.

None of the three fits this window, for the reasons the brief gives and this pass confirmed: `top` is capped, drops unmeasured runs and carries no driver; `task` is one work item at execution grain; `window` is aggregates only. The projection (`COST_RECORD_COLUMNS` in `cost_report_db.rs`) already selects `we.driver` and `wr.usage_unavailable` (added with #3799) but omits the launch-tuple `we.model` and `we.effort_level`, `t.kind`, `t.parent_task_id`, `t.product_id`, `t.project_id`, `t.pr_url`, `t.completed_at`, `wr.finished_at`, execution `started_at`/`finished_at`, `rounds`, the 5m/1h cache split and `host_id`. Everything this window needs is in the schema (`engine/work-db/src/schema_baseline.rs`: `work_runs`, `work_executions`, `tasks`); nothing is captured today that needs a new column.

### Finding 1: the existing projection silently drops executions that have no task

`cost_records_for_window` (`engine/work-db/src/cost_report_db.rs`) inner-joins `work_executions` to `tasks` on `we.work_item_id`. Three execution kinds have no `tasks` row by design: `answer_agent` and `pr_review_guide` set `work_item_id` to a comment or comparison id (`protocol/src/types/execution.rs:44-70`), and `automation_triage` "has no task row to supply a pin" (`engine/work-db/src/driver_lookup.rs`). Their runs carry tokens like any other, and `boss cost window` does not count them. No recorded reason for the inner join was found; the stronger reading is that the taskless kinds were never considered. This window's projection uses a left join and shows those runs under a "no task" group, so the window total is the window total. Whether to switch the three existing verbs to the same projection is an open question below.

### Finding 2: the launch tuple records driver, model and effort, but not reasoning

`work_executions.driver`, `.model` and `.effort_level` are written once at spawn (`engine/work-db/src/execution_launch_config.rs`) and are the historical truth for grouping by driver, model and effort. There is no `work_executions.reasoning`. `tasks.reasoning` is the task's _current_ setting, which the operator can change after the fact. Grouping by reasoning can therefore only be by current value, and the existing `CostBucket` doc already says so. This design labels the reasoning dimension "current setting" in the UI and files recording it at spawn as a deferred entry.

### Finding 3: no app cost surface exists; the Metrics window is the precedent

`rg -i cost app-macos/Sources` finds no token or cost rendering. The precedent for a single-instance secondary window is `Window("Metrics", id: "metrics")` (`BossMacApp.swift:189-193`) with its Window-menu command (`MetricsCommand`, :280-297) holding `@AppStorage` visibility and a shortcut. The engine round trip for a one-shot request is `EngineClient+Requests.swift` (`sendMetricsListLive`, :231) → `EngineClient.swift` parse (:1330-1334) → `EngineEvent` → `ChatViewModel+EventHandling.swift` (:406). Swift Charts is already imported (`MetricsViewer.swift:1`). SwiftUI `Table` is used in `LogViewer.swift:679-714`, flat; no hierarchical table exists. The app's minimum macOS is 15.0 (`app-macos/BUILD.bazel:203`), so `Table(..., children:)` and `DisclosureTableRow` are available. The secondary-window screenshot path is `BossMetricsCapture.swift` (`--capture-metrics` on an isolated instance).

### Finding 4: retention bounds older windows

`execution_retention.rs` prunes only never-started executions in `abandoned`, `failed`, `orphaned` or `cancelled` status that are older than 14 days and outside the five most recent never-started prunable rows per work item. Executions that had a worker run (`started_at` set) are kept indefinitely, and `completed` is never pruned. Older windows therefore under-count those never-started abort rows, not token-bearing failed work. The series (and rows) request emits a `retention_bounded` coverage note whenever the window starts before now minus 14 days, worded to that eligibility.

### Finding 5: `total_tokens` is not comparable across input conventions

`cost_report.rs` sums `input + output + cache_creation + cache_read` for `total_tokens` and ranks by it. That is right when `input` excludes cache reads (**net**) and double-counts them when `input` already contains them (**gross**). Since #3799 only Codex is gross: it preserves the provider's input field as reported (`run-token-usage.md`). Claude is net; Grok is net (ACP `inputTokens` minus `cachedReadTokens`, `run_cost/native.rs::grok`); Antigravity is net (planner-step fresh input, pinned by `captured_agy_steps_report_fresh_input_and_unknown_cache_creation`). This window carries `input_convention` per run (`net` for Claude, Grok and Antigravity, `gross` for Codex, `unknown` when the driver is not recorded or not one of the four) and labels any total or sort that mixes conventions. No driver other than Codex is `capture_unnormalised`.

### Finding 6: the cost-attribution deliverables have not landed

No `cost_capture_status` column and no `work_run_model_usage` table exist on `main` (`rg` over `engine/` and `protocol/` finds neither). One piece of the reserved capture status has landed: #3799 added `work_runs.usage_unavailable`, a durable free-text reason a run has no, or incomplete, provider usage (every new run starts with `No provider token usage observed`; a valid observation clears it). The wire shapes below carry it per run as `usage_unavailable` and as a `usage_unavailable` caveat, and carry `models` as a list (one element today, from `work_runs.model`), so an enumerated capture status and per-model rows flow in as data rather than as a UI change.

### Finding 7: `completed_at` is a transition stamp with a backfill

`tasks.completed_at` is set once on the transition to `done` or `archived` and cleared on reopen (`engine/work-db/src/updates.rs`); rows that were already terminal when the column was added were backfilled with `created_at` (`protocol/src/types/task.rs:824-834`). "Tokens per completed task" is therefore only meaningful from that migration forward, and the series coverage carries a `dimension_started` note for it. There is no merge timestamp; `completed_at` is the proxy the brief chose and this design keeps.

### Data volume

The 2026-07-29 brief counted 12,765 `work_runs` rows. Not re-measured. Tens of thousands of rows is small for a windowed SQL fetch; the latency budget below is set to be measured, not assumed.

## Alternatives considered

### A. Extend `GetCostWindowReport` or `GetTopCostConsumers` instead of adding a request

`WindowCostReport` already buckets by kind, model, reasoning and effort; `TopCostReport` already returns run rows. Add the missing columns and a tree to one of them.

**Rejected as the shape; reused as the internals.** The window's unit is a tree with identities (task, execution, run) under a user-chosen grouping; the existing shapes are fixed-bucket aggregates and a capped ranking. Widening either would change what `boss cost` prints and leave the CLI carrying fields it never shows. The `CostMeasurement` type, the `Accumulator` fold and the NULL-versus-zero contract are reused unchanged; the projection is widened (and corrected per Finding 1) rather than duplicated.

### B. Wait for `GetMetricSeries` and express everything as series

The performance-metrics design defines a generic series request with a `tokens` series over the same projection.

**Rejected, for two independent reasons.** The repo owner decided not to wait on #2954, which has been open since 2026-10-11 with no activity. More fundamentally, the drill-down table is not a series: a bucketed, grouped report has no run identities, no chain structure and no per-row caveats, so even a landed `GetMetricSeries` would not serve the table. The over-time view _is_ a series, and the series request below is shaped so it folds into the catalog later with the builder intact (see "Folding into the metrics series contract").

### C. Fetch rows once and aggregate in the app

Return raw run rows and compute task totals, inclusive roll-ups, group sums and normalizers in Swift. Instant regrouping, no round trips.

**Rejected.** It violates the stated constraint (engine owns aggregation), and it would put a second implementation of every honesty rule into Swift: NULL-never-zero, direct-only list totals, inclusive only on chain roots, no gross-against-net ranking, no `$0.00` on an unpriceable set. The performance-metrics design rejected app-side aggregation of a row snapshot (its alternative E) on the same grounds, and the cost-attribution design's presentation invariants are written at the level of "every figure the system shows", which only holds if there is one place that produces figures. The price of the engine-side choice is one round trip per regroup or filter change, against a query whose budget is set at 150 ms below.

### D. Let the app read `state.db`

**Rejected.** The engine is the only process that opens the database; the app, the CLI and every worker go through the socket. The performance-metrics design states the same non-goal. Nothing about this window justifies a second reader.

### E. One request serving both the table and the charts

Return rows and buckets in one reply so the window makes one call.

**Rejected.** The two have different cardinality and different refresh triggers: the table re-queries on a filter or group change and wants identities; the charts re-query on a range change and want cells. One request would ship the full tree on every range change. Two requests over one projection and one builder crate keep the arithmetic in one place without coupling the payloads.

### F. A persisted usage rollup table

**Rejected.** The data is tens of thousands of rows with a primary-key join; the performance-metrics design rejected a rollup table for the same tables at the same volume (its alternative A) and set a measured budget instead. If the budget is missed on the live database, a rollup keyed to run completion is the escape hatch, never a client cache.

## Chosen approach

### 1. The invariant, stated where it is load-bearing

Every number the window displays is a field of an engine reply. The app contains no code path that sums, divides or compares token fields; it contains code that chooses which engine-supplied field to show, formats it under the presentation invariants, and lays out the tree the engine sent. Concretely: the Swift types decoded from the two replies are the only token-bearing types in the app, and a review checklist item for the app PRs is "no arithmetic over those types outside the formatter's unit conversion".

This is the property reviewers should push on, because it decides the request shapes: a reply must carry every figure at every level the UI can show, including per-group subtrees, so the app never has to derive one.

### 2. `GetTokenUsageRows`: the tree for a window

```text
FrontendRequest::GetTokenUsageRows(TokenUsageRowsRequest {   // a newtype variant: the fields sit flat beside `type` on the wire
    since_epoch_s: i64,
    until_epoch_s: i64,                 // half-open on work_runs.created_at, as cost_records_for_window
    filters: Vec<UsageFilter>,          // { dimension, values }: AND across dimensions, OR within one
    group_by: Vec<UsageDimension>,      // zero, one or two levels, outermost first
    roll_up_revisions: bool,            // default true
    estimate_usd: bool,                 // default false
})
  -> FrontendEvent::TokenUsageRows { report: TokenUsageRowsReport }

UsageDimension = product | project | task_kind | execution_kind | spend_class | driver
               | model | effort | reasoning | task_status | execution_status | coverage | host
```

```text
TokenUsageRowsReport {
    since_epoch_s, until_epoch_s, generated_at_epoch_s,
    capture_start_epoch_s, spans_capture_boundary,      // as WindowCostReport
    overall: CostMeasurement,            // direct sum of every run in the reply, taskless included
    coverage: UsageCoverage,             // runs_total = complete + partial + zero + unmeasured; runs_priceable; counts by caveat code; notes
    tasks: Vec<TaskUsageNode>,           // empty when group_by is non-empty
    taskless: Vec<ExecutionUsageNode>,   // executions with no tasks row (Finding 1); empty when grouped
    groups: Vec<UsageGroup>,             // present only when group_by is non-empty
    dimension_values: Vec<UsageDimensionValues>, // observed { value, label, runs } per dimension, for filter chips
    pricing_gaps: Vec<String>,
    truncated: bool, run_cap: u32,       // see limits
}

UsageGroup {
    key: Vec<(UsageDimension, String)>,  // one pair per group_by level
    measurement: CostMeasurement,        // direct sum of the runs in this group
    subgroups: Vec<UsageGroup>,          // second level, if requested
    tasks: Vec<TaskUsageNode>,           // the tree restricted to this group's runs
    taskless: Vec<ExecutionUsageNode>,
}

TaskUsageNode {
    task_id, short_id, name, kind, status, product_id, project_id, pr_url, completed_at_epoch_s,
    reasoning_current, effort_current,   // tasks.reasoning / tasks.effort_level: current settings, labelled as such
    chain_root_id, is_chain_root,
    direct: CostMeasurement,             // this task's own runs in the reply
    inclusive: Option<CostMeasurement>,  // chain roots only: direct + direct of nested revisions
    by_spend_class: Vec<CostBucket>,     // implementation / design / overhead, over direct
    runs_outside_window: u32,            // this task has runs the window does not cover: its figure is a slice
    executions: Vec<ExecutionUsageNode>,
    revisions: Vec<TaskUsageNode>,       // nested when roll_up_revisions; otherwise revisions are top-level siblings
}

ExecutionUsageNode {
    execution_id, kind, spend_class, status, created/started/finished_at_epoch_s, pr_url, host_id,
    driver: Option<String>, driver_source: launch_tuple | resolved | unknown,
    model_launched: Option<String>, effort_launched: Option<String>,
    measurement: CostMeasurement,
    runs: Vec<RunUsageRow>,
}

RunUsageRow {
    run_id, status, created/started/finished_at_epoch_s, host_id, rounds: Option<i64>, agent_active_ms: Option<i64>,
    model_observed: Option<String>,      // work_runs.model verbatim, "<synthetic>" included
    models: Vec<ModelUsage>,             // one element today (observed model, all classes); per-model rows later
    tokens: Option<TokenClasses>,        // None = unmeasured. Each class is itself Option: a class the provider did not report is NULL, not 0
                                         // { input, output, cache_creation, cache_creation_5m, cache_creation_1h, cache_read }
    total_tokens: Option<i64>,           // sum of the reported classes (a floor when partial); None = unmeasured. The app never adds classes itself
    input_convention: net | gross | unknown,
    coverage: measured | partial | unmeasured | zero,   // partial: some classes reported, some not (matches CostMeasurement.tokens_partial)
    usage_unavailable: Option<String>,   // work_runs.usage_unavailable verbatim: why the run has no, or incomplete, usage; None = observed
    caveats: Vec<UsageCaveat>,           // { code, detail }
    estimated_usd: Option<f64>, unpriceable_cause: Option<String>,
}
```

**Semantics the builder enforces, each pinned by a unit test:**

- **Window.** A run is in the reply iff `work_runs.created_at ∈ [since, until)`. A task appears iff at least one of its runs, or one of its nested revisions' runs, is in the reply. `runs_outside_window` is the count of the task's runs not in the window, so the UI can mark a task figure as a slice rather than a total.
- **Direct and inclusive.** `direct` sums a task's own runs. `inclusive` exists only on a chain root and equals `direct` plus the `direct` of every revision nested under it. `overall` and every `UsageGroup.measurement` sum direct only; the test asserts `overall == Σ direct over tasks and taskless` and `overall == Σ group measurements` at each level. This is the cost-attribution design's Layer 4 rule: inclusive is for one row's detail, never for a list total.
- **Chain roots.** For each distinct `kind = 'revision'` task in the fetched rows the builder resolves the root with the existing `chain_roots` walk (`engine/work-db/src/chain_helpers.rs`), which handles legacy nesting and broken parents. A root with no runs of its own in the window is still emitted as a node (with an empty `executions` list and `direct.runs_total = 0`) so its revisions have somewhere to nest. Follow-up provenance (`origin_task_short_id`, `origin_pr_number`) is not chain linkage and is ignored.
- **Taskless executions.** `answer_agent`, `pr_review_guide` and `automation_triage` runs are fetched by left join and emitted under `taskless`, grouped by kind in the UI as "Engine overhead (no task)". They count in `overall`.
- **Driver.** `work_executions.driver` when present (`driver_source = launch_tuple`). When that column is NULL, the projection fills the slug from the same chain `driver_transcript::resolve_execution_driver_slug` uses (`tasks.driver` → product default → pool policy → engine default) and sets `driver_source = resolved`. A NULL that still does not resolve is `unknown`, rendered as "not recorded", never as Claude. Resolution is once per distinct NULL-driver execution in `usage_records_for_window`, after the window snapshot is released. `resolved` is the task's _current_ routing, not a frozen record of what ran; the launch tuple always wins when it is present.
- **Input convention.** `net` for `claude`, `grok` and `agy`, `gross` for `codex` only, `unknown` otherwise (Finding 5). `TokenClasses` are carried verbatim; the builder does not subtract. `CostMeasurement.total_tokens` keeps its existing definition, and the per-run caveat `gross_input` is what tells the UI to label it.
- **Coverage states.** Per run: `measured` (every class reported, at least one non-zero), `partial` (at least one class reported and at least one not: Grok and Antigravity never report cache creation), `zero` (every class reported and all exactly zero) and `unmeasured` (no class reported). Every class in `TokenClasses` is an `Option`, so a partial run's unreported class stays NULL on the wire and in every aggregate (`cache_creation_tokens` is unknown on a measurement when any measured run lacks it, with `tokens_partial` set). `UsageCoverage` counts the four states exclusively.
- **Caveats** are engine-emitted, closed-enumeration codes with free-text detail, so a new capture-quality signal is a new code and a one-line addition to the app's label table, not a layout change. v1 codes: `pre_capture` (created before `TOKEN_CAPTURE_START_EPOCH_S`), `driver_not_recorded`, `gross_input` (Codex), `subagents_excluded` (Claude runs that have tokens), `synthetic_model` (detail: the launched alias), `model_from_launch_tuple` (observed model missing; `model_launched` shown as a fallback and marked as an alias), `tokens_partial` (detail: the classes not reported), `usage_unavailable` (detail: `work_runs.usage_unavailable` verbatim, so the stored reason is the per-run capture status without a second mapping), `unpriced_model`, `estimate_only` (attached to every USD figure). There is no `driver_no_capture`: Grok and Antigravity record tokens since #3799. The app renders an unknown code by its literal string so an engine ahead of the app still shows something true.
- **USD.** Only when `estimate_usd` is set. Priced through the existing `cost_pricing` seam, handed to the builder as a small estimator trait so the builder crate does not depend on the crate that owns the rate table. A run is priced only when it is measured, its driver is not subscription-billed, its `input_convention` is `net`, every class is reported and its observed model has a complete price. Every other run is `unpriceable`, with the first applicable cause: `unmeasured`, `subscription_unpriced` (driver `grok` or `agy`: excluded **by driver**, so an Antigravity run that names a model the API table prices is still not priced), `driver_unknown`, `capture_unnormalised` (gross input, i.e. Codex), `tokens_incomplete` or `unknown_model`. Priced-eligible unpriceable measured runs (every cause except `subscription_unpriced`) set `estimated_usd_partial` and name their model in `pricing_gaps`; `subscription_unpriced` runs are excluded by driver without marking aggregates partial or emitting a pricing-gap warning. Nothing priced is `None`, never `$0.00`; `coverage.runs_priceable` counts the priced runs. This applies the cost-attribution design's entry 24 rule inside the new request without touching the three existing verbs. When that design replaces `cost_pricing`, this request's pricing call moves with it.
- **Grouping.** A run's dimension values come from the execution (driver, model via the observed run model falling back to launched, effort launched falling back to task current), the task (product, project, kind, status, reasoning current) or the run (coverage, host). When `group_by` is non-empty the tree is emitted once per group, restricted to the runs matching that group's key, so a task with a Claude execution and a Codex execution appears under both driver groups with only the matching executions beneath. Each run appears exactly once at the leaf level, which is why group measurements add up to `overall`.
- **Dimension values** carry a display label (product and project names, kind titles) and the run count, so filter chips are built from data, not hardcoded in Swift. Each dimension's counts apply every active filter _except that dimension's own_, so a chip for a value that is currently filtered out still shows what selecting it would add; counting under the dimension's own filter would hide the alternatives and make OR-within-a-dimension impossible to build. A filter with no values restricts nothing.
- **Spend classes** are `implementation`, `design`, `overhead` and `unclassified`. The cost-attribution table predates `pr_review_guide` and `progression`; both are `overhead`. An execution kind string the engine build does not recognise is `unclassified` rather than folded into one of the three.
- **Limits.** The reply is capped at a run count the engine owns (5,000 runs, `DEFAULT_RUN_CAP`; the first proposal was 20,000 and the measurement under "Latency budget" below moved it; the brief's whole-history count was 12,765 in July). Past the cap the builder keeps the most recent runs, sets `truncated: true` and the UI tells the operator to narrow the window. Nothing is dropped silently.
- **Coverage notes** at report level mirror the series request: `capture_started` when the window spans the boundary, `retention_bounded` when it starts more than 14 days ago, `pricing_gaps` when a USD estimate excluded models.

### 3. `GetTokenUsageSeries`: buckets with normalizers

```text
FrontendRequest::GetTokenUsageSeries {
    since_epoch_s, until_epoch_s,
    bucket: Option<"hour" | "day" | "week" | "month">,   // None: engine picks as the metrics design does
    group_by: Option<UsageDimension>,
    filters: Vec<UsageFilter>,
    estimate_usd: bool,
}
  -> FrontendEvent::TokenUsageSeries { report: TokenUsageSeriesReport }

TokenUsageSeriesReport {
    since_epoch_s, until_epoch_s, bucket_width, bucket_secs, generated_at_epoch_s,
    groups: Vec<String>,                 // keys in render order, "(all)" when ungrouped
    buckets: Vec<UsageBucket>,           // ascending; a bucket with no fact is absent
    coverage: UsageSeriesCoverage,       // data_from_epoch_s, dimension_from_epoch_s, notes
}
UsageBucket { start_epoch_s, cells: Vec<UsageCell> }
UsageCell {
    group: String,
    measurement: CostMeasurement,                // runs created in the bucket, direct
    share_of_bucket_total_tokens: Option<f64>,   // this group's measured total / bucket's measured total
    executions_completed: u32,                   // status completed, finished_at in bucket
    tasks_completed: u32,                        // tasks.completed_at in bucket
    tasks_completed_cost: CostMeasurement,       // inclusive chain cost of those tasks, whenever spent
    distinct_prs: u32,                           // distinct pr_url over executions finished in bucket
    ratios: UsageRatios {
        tokens_per_run, tokens_per_completed_execution, tokens_per_active_hour,
        tokens_per_completed_task, tokens_per_pr: Option<f64>,
    },
}
```

**Ratio rules.** A ratio is `None` when its denominator is zero or when its numerator has no measured runs; the app draws no point, never a zero. `tokens_per_run` and `tokens_per_completed_execution` divide the bucket's measured `total_tokens` by `runs_measured` and `executions_completed`. `tokens_per_active_hour` restricts **both** numerator and denominator to measured runs that have `agent_active_ms` recorded (Grok and some Antigravity runs report tokens without active time), dividing their tokens by `agent_active_ms / 3.6e6`, and emits an `active_time_missing` coverage note counting how many measured runs were excluded. `tokens_per_completed_task` divides `tasks_completed_cost.total_tokens` by `tasks_completed` and is the one normalizer whose numerator can include runs outside the window, by design: it answers "what did the tasks finished this week cost, all in", and its label says so. `tokens_per_pr` divides the bucket's measured total by `distinct_prs` and is labelled as an approximation for the same reason in reverse.

**How the view answers the owner's question.** More work shows as runs, completed executions and completed tasks rising while per-run and per-task tokens stay flat. More expensive work shows as per-run, per-active-hour and per-task tokens rising. A mix shift shows as `share_of_bucket_total_tokens` moving between model or driver groups while per-run figures within each group stay flat. A driver-mix shift also changes how comparable the totals are, which the `gross_input` caveat count in coverage makes visible.

**Honest range handling** follows the performance-metrics design verbatim: `data_from_epoch_s` is the later of the earliest fact and `TOKEN_CAPTURE_START_EPOCH_S`; `dimension_from_epoch_s` for driver, model and effort is the launch-tuple migration era, and for `tokens_per_completed_task` it is the `completed_at` migration (Finding 7); notes are `capture_started`, `dimension_started`, `retention_bounded`, `active_time_missing`, `pricing_gaps`, `bucket_coarsened`. Bucket sums preserve per-class NULL handling (a bucket whose measured runs lack a class reports that class as unknown with `tokens_partial`, never zero). The app shades the region before `data_from` (or `dimension_from` when grouped) and never draws a line through it.

### 4. Folding into the metrics series contract

When the performance-metrics design's `GetMetricSeries` lands, `TokenUsageSeriesReport` maps onto `MetricSeriesReport` directly: `measurement` is its `Tokens`/`Usd` value kinds, and `executions_completed`, `tasks_completed`, `distinct_prs` and each ratio become catalog series (`Count` and a new `Ratio` value kind) over the same projection. The series builder in this design is a pure function from projected rows to cells; the fold is a new handler that re-wraps its output into the catalog shape and a one-line catalog entry per series, with the builder and its tests unchanged. The rows request does not fold; it is not a series and stays as the table's contract. The implementer of the fold should delete `GetTokenUsageSeries` only once the app's chart view consumes the catalog shape, so there is never a period with two series contracts serving one view.

### 5. Engine placement

- **A new crate, `tools/boss/engine/usage-report`** (`boss-usage-report`), holding the projected record type, the rows builder, the series builder, caveat and spend-class classification, and their tests. It depends on `boss-protocol` only. The work database depends on it for the record types and `engine/app` for the builders, never the reverse, so the edges are one-directional. The rate table stays in `engine/core` (`cost_pricing.rs`) and reaches the builders as a small estimator trait the handler implements over it; the capture-start instant and the retention bound are likewise handed in as query data, so the crate owns none of the engine's constants and nothing is defined twice. This follows the repo's prefer-crates convention: the unit has its own vocabulary, two consumers (the handler and the work database's projection, plus the CLI through the wire types) and a test surface that should not rebuild `engine/core`. `cost_report.rs` stays where it is, and so does its accumulator: the new crate folds with its own accumulator over the richer record, and a test in `engine/app` pins that its `overall` equals `boss cost window`'s for the same runs, so the two cannot drift silently. Consolidating them is a move, not a rewrite, and is not this project's job.
- **Projection.** `engine/work-db/src/cost_report_db.rs` gains `usage_records_for_window(since, until)` returning the window's runs (each joined to its execution, so a run whose work item has no `tasks` row is kept) plus the task context of every task they reference and each one's chain root, read in one snapshot. Chain roots come from the engine's existing `chain_roots` walk, so legacy nesting and broken parents resolve exactly as elsewhere. The predicate is half-open on `CAST(wr.created_at AS INTEGER)` and the migration indexes that same expression (`work_runs_created_at_epoch_idx`, the shape `work_executions_finished_at_idx` already uses), so SQLite can seek; this replaces the earlier idea of comparing 10-digit text, which would silently mis-order any timestamp that is not exactly ten digits. The migration is `CREATE INDEX IF NOT EXISTS`, so it is a no-op if the performance-metrics design (#2954) adds the same index first. A query-plan test pins that the window read uses it.
- **Handlers** in `engine/app/src/app/cost.rs`, thin fetch-then-build shims beside the three cost handlers. Every request's replay, admission lane, worker access and progression access are chosen in one exhaustive match, `protocol/src/request_contract.rs`, so a request cannot compile without them; both requests go in the arm that holds the three cost requests (bulk lane; denied to workers and progression, as operator and coordinator diagnostics).
- **Latency budget.** 150 ms p95 for the rows request over a 90-day window and for the series request over the full history at day buckets, measured on the operator's machine against the live database size and recorded in the PR. If missed, the first lever is the index above; the second is the rollup escape hatch in alternative F.

  **Measured (2026-11-23, re-measured after compiling filters once, borrowing dimension values, and deferring caveats/pricing past the 5,000-run cap): the 150 ms p95 budget is still missed on a 90-day synthetic window, and the database read is not why.** The live database could not be read (workers never touch the engine's own state), so the request was measured against an isolated engine (default Bazel fastbuild binary) on a synthetic database of production order: 60,000 runs, 24,071 executions and 6,026 tasks over 120 days (the seed fixture plus a synthetic fill), about 500 runs a day. The 90-day window contained 45,004 runs and **1 distinct NULL-launch-tuple execution** (from the seed fixture; the synthetic fill populates `work_executions.driver`, so `fill_resolved_drivers` resolution cost for historical pre-launch-tuple windows is unmeasured here). `DEFAULT_RUN_CAP` kept the most recent 5,000 runs (`truncated: true`). Because the host was shared, the figure used is the engine process's own CPU time per call (`ps` cputime delta), the nearest stable proxy for uncontended latency. Twenty `boss cost rows --since 90d --json` calls against that isolated engine: CPU samples `1130, 1130, 1150, 1150, 1150, 1150, 1160, 1160, 1160, 1160, 1160, 1170, 1170, 1170, 1170, 1170, 1170, 1170, 1170, 1240` ms (nearest-rank p95 **1,170 ms**, mean 1,163 ms, min 1,130 ms, max 1,240 ms; wall p95 2,458 ms). Ten filtered calls (`--filter driver=claude`) on the same window: CPU samples `1160, 1160, 1160, 1160, 1160, 1170, 1170, 1170, 1170, 1180` ms (p95 **1,180 ms**, mean 1,166 ms), confirming active filters add negligible overhead. Pretty-printed CLI JSON was 24.5 MB (compact wire JSON ~12 MB, about 2.5 KB per run in the reply). The cap bounds the reply, not the window scan: every run in `[since, until)` is still fetched and scanned for dimension counts before the newest 5,000 are kept. The window read seeks the new index, so the rollup table is not needed; the cost is scanning the window plus serialising the capped tree. The remaining levers, if the app needs more of either (after checking release-build latency on the live database), are pushing the filter/cap into SQL, omitting `runs` beneath executions until expanded, and batching `fill_resolved_drivers` if many NULL-launch-tuple executions fall inside the window.

  **Measured (series PR, 2026-11-24, after batched driver resolution and skipping in-window extra-run subqueries): 7d day buckets meet the 150 ms p95 budget again; 30d and full history still miss it in unoptimised `fastbuild` Rust.** The pre-window runs projection for finished executions had regressed the design's 7d fallback outside the budget (about 143 ms → 191.9 ms p95) and raised 30d (about 419 ms → 516.9 ms), so the Over-time view had no window inside 150 ms. This pass batches NULL launch-tuple driver resolution into a handful of `IN (json_each(?))` queries (`get_execution_driver_slugs_lenient`) and skips the by-id extra-run subquery for executions and chain members whose `created_at` is already `>= since` for the `< since` half only (a run cannot exist before its execution or task row); their runs at or after `until` (for example a post-merge review of an already-done root, in a window that ended in the past) still come from an index-seeked `created_at >= until` range, which is empty when `until` is now. Measured against an isolated engine (`bazel build //tools/boss/engine/core:engine`, default `fastbuild` config) on a synthetic production-order database (60,020 runs, 24,019 executions, 6,013 tasks over 120 days, seed fixture plus fill; host load average 8.28 / 7.58 / 8.37; 20 calls after 2 warmups for full history and 10 calls after 2 warmups for 30d/7d; `mach_timebase_info`-scaled `proc_pidinfo` engine CPU time):
  - `boss cost series --since 150d --bucket day --json` (full history, ungrouped, 241 KB JSON): engine CPU nearest-rank p95 **1,004.9 ms** (mean 928.6 ms, min 901.5 ms, max 1,015.7 ms; wall p95 1,191.6 ms; samples `1004.9, 1015.7, 906.9, 913.9, 976.7, 937.6, 910.7, 912.0, 919.5, 917.3, 925.3, 906.0, 922.8, 934.3, 918.2, 911.3, 911.1, 911.1, 914.7, 901.5` ms). Still misses 150 ms.
  - `boss cost series --since 150d --bucket day --group-by driver --json` (full history, grouped by driver, 880 KB JSON): engine CPU p95 **985.6 ms** (mean 972.8 ms, min 945.6 ms, max 1,072.1 ms; wall p95 1,095.5 ms; samples `959.2, 967.4, 1072.1, 982.5, 961.2, 966.4, 972.7, 971.8, 985.6, 971.1, 964.3, 957.6, 968.3, 977.2, 945.6, 979.1, 954.4, 957.7, 969.8, 972.2` ms). Still misses 150 ms.
  - Shorter ranges on the same 60,020-run database (10 calls after 2 warmups): 30d day buckets (59 KB JSON) engine CPU p95 **241.2 ms** (mean 234.9 ms, min 231.1 ms, max 241.2 ms; wall p95 255.1 ms) — improved from the 516.9 ms regression, still outside the budget. 7d day buckets (16 KB JSON) engine CPU p95 **114.2 ms** (mean 98.2 ms, min 62.0 ms, max 114.2 ms; wall p95 229.7 ms) — inside the 150 ms CPU budget after the 191.9 ms regression; this is the design's fallback range, so the Over-time view again has a window that meets 150 ms. Wall p95 on 7d was higher because the host was loaded; the budget figure is engine CPU.
    The reply payload stays 60× smaller than `rows`, so serialisation is not the cost; the dominant remaining cost is projecting and classifying all 60,000 runs in unoptimised `fastbuild` Rust. Next levers for the remaining full-history and 30d gap: measuring a release (`-c opt`) build, adding a `work_executions(CAST(finished_at AS INTEGER)) WHERE finished_at IS NOT NULL` index (for narrow ranges), and the terminal-transition rollup escape hatch in alternative F.

- **An exercised caller.** `boss cost rows --since <t> [--until <t>] [--group-by d[,d]] [--filter dim=a,b] [--no-roll-up] [--usd] [--json]` and `boss cost series ... [--bucket ...]` render the wire shapes (`--json` verbatim, text as a compact table) so the contracts have a caller before the app lands, and so the screenshot recipe can be checked without the app. The text renderers reuse the invariants `cli/src/cost_cmds.rs` already honours ("unknown" never `0`; "ESTIMATE, not billing truth" on every USD figure). `boss cost rows` fulfils the cost-attribution design's planned `boss cost runs` drill-down; that entry should be re-pointed at it when scheduled.
- **A seed fixture for isolated engines.** Token columns are written only by the capture path (`run_cost.rs:426` → `set_run_cost_snapshot`), so an isolated engine has no usage to show. A checked-in SQL fixture under the new crate's `fixtures/` inserts products, projects, tasks (including a revision chain and taskless executions), executions across all four drivers (claude, codex, grok, agy) and the launch-tuple era boundary, and runs covering every coverage and caveat case: pre-capture NULLs, measured zero, `<synthetic>`, a Codex gross-input run, a measured Grok run (no cache creation, no `agent_active_ms`), an Antigravity run whose model name an API table could match, a run carrying `usage_unavailable`, a run with no observed model, and unsplit and split cache writes. It is applied with `sqlite3` to the fixture database derived from the isolated socket path (`app/isolation.rs`: `/tmp/boss-test-UUID.sock` → `/tmp/boss-test-UUID.db`) after the engine has created the schema; the recipe is the header of `fixtures/token-usage-seed.sql`, and a test in the work database applies the file to a freshly migrated schema so it cannot drift. That database is a test fixture; it is never the production state under the Boss application-support directory, which workers must not touch.

### 6. The Token Usage window

**Registration.** `Window("Token Usage", id: "token-usage")` in `BossMacApp.swift` next to Metrics, with a `TokenUsageCommand` in the `CommandGroup(after: .windowList)` block holding `@AppStorage("boss.tokenUsage.visible")` and `Cmd+Shift+O` (Cmd+Shift+K, L, M, T and U are taken by CommentLayer, Logs, Metrics, Terminal Loop and UI Stalls). Default size 1180×720, minimum 900×520.

**Layout.** A toolbar row, a hierarchical table, an inspector on the right, and a footer.

- **Toolbar.** Range presets (24h, 7d, 30d, 90d, All) plus a custom range; a group-by picker with up to two levels (None, Product, Project, Task kind, Execution kind, Spend class, Driver, Model, Effort, Reasoning (current), Task status, Execution status, Coverage, Host); filter chips per dimension populated from `dimension_values` with the run count in each chip; a "Roll up revisions" toggle (default on); an "Estimated USD" toggle (default off); a refresh button and a staleness pill after 60 s, as the Metrics window does.
- **Table.** SwiftUI `Table` with `children:` over a node type the app builds by walking the reply: group → task → (nested revision tasks) → execution → run; taskless executions under "Engine overhead (no task)" by kind. Columns: Name (short id, name, kind badge; execution rows show kind and status; run rows show run status), Driver (with `driver_source` in the tooltip), Model (observed; launched model and effort in the tooltip; synthetic marker), Effort, Reasoning (current), Status, Runs (`measured/total`), Input, Output, Cache write (5m/1h split in the tooltip), Cache read, Total (with a `gross` marker when the row's or any descendant's `input_convention` is gross), Active time, Est. USD (hidden unless enabled), Coverage (badge: complete / partial with ratio / none with reason), Caveats (icons; tooltip lists the codes' labels). Column sorting uses `sortOrder` with `KeyPathComparator`s; sorting applies within each sibling set, never across levels. Sorting by Total while rows mix input conventions shows a one-line inline note above the table rather than refusing.
- **Inspector.** For the selected node: for a task, direct and inclusive side by side (inclusive bold on a chain root, absent otherwise), the spend-class split, `runs_outside_window` as "N more runs outside this range", the PR link, and "Reveal on board"; for an execution, the launch tuple, timestamps, host, PR; for a run, every token class, the per-model list, rounds, active time, `usage_unavailable` ("not recorded" while NULL), and each caveat spelled out in a sentence.
- **Footer.** `overall` for the current filters with its coverage triple, and the report-level notes: a banner when the window spans the capture boundary ("totals are a floor: runs before 2026-07-27 have no token data"), a note when retention bounds the range, a truncation notice when `truncated` is set.

**Presentation invariants.** One Swift formatter owns every figure; unit tests pin each row of this table, which restates the cost-attribution design's "what the system shows where it cannot attribute" for the app:

| Situation                       | Shown                                                                                                                                                                                                                                                         |
| ------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Unmeasured run                  | `—` with "not instrumented" (pre-capture) or the per-run `usage_unavailable` reason (post-capture NULL). Never `0`. Never a Grok-specific "records nothing" string.                                                                                           |
| Measured zero                   | `0`, coverage complete. The only path to a displayed zero.                                                                                                                                                                                                    |
| Class unknown on a measured run | That cell only is `—` / "unknown" (e.g. Grok and Antigravity cache creation). Distinct from a whole-node unmeasured `—` / "not recorded". Never `0`.                                                                                                          |
| Node with partial coverage      | The figure, the `measured/total` ratio adjacent, and a "floor" label.                                                                                                                                                                                         |
| Node with no measured runs      | `—` and the reason (the dominant caveat, or `usage_unavailable` verbatim).                                                                                                                                                                                    |
| USD                             | `est. $x.xx`; Grok and Antigravity show `unpriced (subscription)` (`subscription_unpriced`), never `$0.00` and never a pricing-gap warning; other unpriceable causes hide the figure; marked partial when any priced-eligible run in the set was unpriceable. |
| `<synthetic>` model             | The literal string with a warning icon; launched model in the tooltip. Never replaced.                                                                                                                                                                        |
| Gross-input driver              | `gross` marker on Total; sort-by-Total note when conventions mix.                                                                                                                                                                                             |
| Chain root                      | Inclusive bold beside direct; list and footer totals sum direct only.                                                                                                                                                                                         |
| Window spans capture boundary   | Footer banner; every figure labelled a floor.                                                                                                                                                                                                                 |

**Errors.** A failed or timed-out request shows an inline banner inside the window and keeps the last reply dimmed; no alert, per `app-macos/AGENTS.md`.

**Refresh.** On appear, on every toolbar change (debounced 150 ms, with a generation counter so a stale reply is dropped), on the refresh button, and on a 60 s timer while visible. No subscription to work topics; per-item invalidations are the wrong grain for a windowed report.

**Screenshots.** A `--capture-token-usage` path mirroring `BossMetricsCapture.swift` hosts the window's root view offscreen on an isolated instance, waits for the rows reply, captures via `BossWindowCapture`, and exits. Each UI PR runs it against an isolated engine seeded with the fixture from section 5, captures at least: the ungrouped tree with a revision chain expanded, grouping by driver then model, the footer banner on a window spanning the capture boundary, and the inspector on a `<synthetic>` run; and registers the PNGs with captions. The PR body states what each screenshot verifies and what could not be captured.

### 7. The over-time view

A second tab in the Token Usage window ("Over time"; the table is "Breakdown"), sharing the toolbar's range, filters and group-by (single level in this tab). Chart cards in a two-column grid, each a Swift Charts view over `TokenUsageSeriesReport` cells:

- **Tokens** by class (stacked `AreaMark`) or by the chosen group.
- **Work volume**: runs, completed executions, completed tasks, distinct PRs (`BarMark`s on a shared axis).
- **Normalized**: one `LineMark` per group for the selected normalizer (picker: per run, per completed execution, per active hour, per completed task, per PR), with a `None` cell leaving a gap.
- **Mix**: `share_of_bucket_total_tokens` per group as a stacked 100 % area.
- **Estimated USD** when enabled, with the unpriceable share drawn hollow and the title saying "estimated".

Every card shades the pre-coverage region and draws no line through it; hover shows every group's value for the bucket via `chartXSelection`. Range changes re-query with the same debounce and generation rule as the table. Brush, pinch and the mini-map are not built here (non-goal); when the performance-metrics design ships them for the Metrics window, this tab adopts the same overlay.

### 8. Forward compatibility with the cost-attribution project

- `RunUsageRow.usage_unavailable` is the per-run capture status today (`work_runs.usage_unavailable`, verbatim, plus a `usage_unavailable` caveat). When `work_runs.cost_capture_status` lands, the projection selects it and the builder maps each status to a caveat code (`skipped_containment_unresolved` → `capture_skipped`, and so on) beside the existing one. No wire or UI change.
- `models` on `RunUsageRow` is one element from `work_runs.model` today; when `work_run_model_usage` lands the projection reads the child table and emits one `ModelUsage` per row, and the inspector's per-model list simply has more entries.
- `TokenClasses` carries the 5m/1h cache split now so the disjoint vocabulary of that design's Layer 1 is a rename, not an addition.
- Pricing stays behind the `cost_pricing` call in the builder, so replacing that module replaces this request's USD path.

## Risks / open questions

- **The round-trip-per-regroup choice is the design's bet.** If the live database makes the rows request slower than the 150 ms budget, the toolbar will feel laggy. Mitigations in order: the `work_runs(created_at)` index, narrowing the default range to 7 days, and the rollup escape hatch. What is not a mitigation is moving sums into Swift.
- **Reply size.** At about 2.5 KB of compact wire JSON per run (~12 MB at the 5,000-run cap; see the Measured figures in §5 "Latency budget"), a 20,000-run cap would exceed 45 MB and seconds of engine CPU, which is why the cap is 5,000. Even at 5,000 runs, scanning a 90-day window of 45,004 runs and serialising the capped tree exceeds the 150 ms p95 budget on fastbuild engine CPU. The lever, if the app needs lower latency or more rows, is omitting `runs` beneath executions until expanded (an `expand: execution_ids` request field), which keeps the invariant intact and removes the largest single share of the payload; compacting the repeated `CostMeasurement` on every task and execution node is the second.
- **Grouping by model mixes an observed string with a launched alias.** When `work_runs.model` is NULL the builder falls back to `work_executions.model` (an alias like `opus`) and marks the run `model_from_launch_tuple`; the two can land in different groups for the same billable model until the cost-attribution design's canonicalisation lands. Shown, not hidden.
- **`tokens_per_completed_task` attributes all-time chain cost to the completion bucket.** This is the intended definition, but it makes the series non-additive across buckets with the `Tokens` card. The card title and the inspector say so; reviewers should confirm this is the normalizer the owner wants rather than "tokens of runs in bucket ÷ tasks completed in bucket", which is additive but answers a different question.
- **Switching the three existing `boss cost` verbs to the corrected projection** would change their output (adding taskless runs). Recommended, as one projection is better than two, but it is a visible behaviour change to a shipped CLI and is asked as a question.
- **Reasoning is not in the launch tuple** (Finding 2). Grouping by reasoning is by current setting and labelled so; the deferred entry records it at spawn if the owner wants historical accuracy.
- **Retention.** Never-started prunable executions older than 14 days (past the per-work-item floor) are gone; executions with worker runs are kept. The `retention_bounded` note is the only trace of the pruned never-started rows. The window cannot show what was pruned.
- **Hierarchical `Table` on macOS 15** has known rough edges (expansion state on reload, per-level sorting). The app PR should build the node tree as a pure function of the reply with its own unit tests so the view layer stays thin, and should keep expansion state keyed by node id across reloads.

## Proposed implementation task breakdown

Breakdown size: 5 entries (4 in-scope, 1 deferred) — the change touches two subsystems (engine/protocol and the macOS app) with two real contracts (the rows request and the series request), each contract paired with the app surface that consumes it; the two engine entries share one new crate and one projection and are serialised, the two app entries share one window and are serialised, and the second engine entry runs in parallel with the first app entry. The deferred entry records one capture gap this analysis surfaced. Parallelism: entries 2 and 3 run concurrently (app files versus engine files, no overlap); entry 4 waits for both.

### Engine: usage rows request, projection, crate, CLI caller and seed fixture

Scope: create the `boss-usage-report` crate with the projected record type, the rows builder (window, direct/inclusive, chain-root resolution, taskless executions, grouping to two levels, caveats, partial coverage, USD that excludes subscription-billed drivers and gross input, truncation) and its tests; widen the projection in `engine/work-db/src/cost_report_db.rs` with a left join to `tasks` and the additive `work_runs(created_at)` index migration; add `GetTokenUsageRows` / `TokenUsageRows` to `protocol` with the read-admission and worker-policy registrations; add the thin handler; add `boss cost rows` as the exercised caller; add the SQL seed fixture and a short recipe for applying it to an isolated engine. Measure and record the latency budget on the live database size.

Deliverables:

- `tools/boss/engine/usage-report` crate with builders and tests pinning: NULL-never-zero, `overall == Σ direct`, group sums equal `overall` at each level, inclusive only on chain roots, a root with no own runs still emitted, taskless runs counted, truncation flagged, caveat codes per case.
- Protocol types and wire variants; lane and policy registration.
- `usage_records_for_window` projection and the index migration.
- `boss cost rows` with `--json` and text output honouring the existing CLI invariants.
- `fixtures/token-usage-seed.sql` and the recipe; PR body shows `boss cost rows --json` output against the seeded isolated engine and the measured p95.

Effort: large

Depends on: none

Scope: in-scope

### App: Token Usage window with the drill-down table, filters, grouping, chain roll-up and inspector

Scope: the window, its Window-menu command and shortcut, the decoded reply types, the pure tree-builder from reply to table nodes, the hierarchical `Table` with per-level sorting, filter chips from `dimension_values`, the two-level group-by picker, the revision roll-up toggle, the inspector, the footer with coverage and notes, the single figure formatter with unit tests pinning every presentation invariant, inline error banner, debounced refresh with generation counter, and the `--capture-token-usage` capture path. Screenshots against the seeded isolated engine as listed in section 6, registered with captions and described in the PR body.

Deliverables:

- `TokenUsageWindow` and command registration in `BossMacApp.swift`; request/parse/event plumbing in `EngineClient+Requests.swift`, `EngineClient.swift`, `ChatViewModel+EventHandling.swift`.
- Pure tree builder and figure formatter with tests in `Tests/BossTests`.
- Capture path and at least four registered screenshots; PR body states what each verifies.
- Review checklist item satisfied: no arithmetic over token fields outside the formatter's unit conversion.

Effort: large

Depends on: Engine: usage rows request, projection, crate, CLI caller and seed fixture

Scope: in-scope

### Engine: usage series request with normalizers and coverage notes

Scope: the series builder in `boss-usage-report` (bucketing with engine-chosen width, per-cell measurement, completed executions, completed tasks with attributed inclusive cost, distinct PRs, share of bucket, the five ratios with `None` on zero denominators), `UsageSeriesCoverage` with `data_from`/`dimension_from`/notes including `retention_bounded` and the `completed_at` migration boundary, `GetTokenUsageSeries` / `TokenUsageSeries` on the wire with lane and policy registration, the handler, `boss cost series` as the exercised caller, and the fold-in note in code comments pointing at the metrics catalog. Runs in parallel with the app table entry: it edits engine and protocol files only.

Deliverables:

- Series builder and tests pinning: absent bucket for no facts, `None` ratios on zero denominators, attributed task cost includes out-of-window runs, coverage notes per boundary, bucket coarsening at the cell cap.
- Protocol types and wire variants; lane and policy registration; handler.
- `boss cost series` output against the seeded isolated engine in the PR body with the measured p95 at day buckets over full history.

Effort: medium

Depends on: Engine: usage rows request, projection, crate, CLI caller and seed fixture

Scope: in-scope

### App: over-time tab with normalized charts

Scope: the "Over time" tab sharing the toolbar, the series request plumbing, the five chart cards (tokens by class or group, work volume, normalized with the normalizer picker, mix, estimated USD when enabled), pre-coverage shading, hover values, gaps for absent buckets and `None` ratios, debounced re-query on range change, and screenshots against the seeded isolated engine showing each card, a grouped normalized chart, and the shaded pre-coverage region on an all-history range.

Deliverables:

- Tab, cards and chart renderers keyed by the reply's shape, with a pure cell-to-series mapper tested in `Tests/BossTests`.
- Capture path extended to the tab; registered screenshots with captions; PR body states what each verifies.
- No arithmetic over token fields in the app (the ratio and share fields come from the reply).

Effort: medium

Depends on: App: Token Usage window with the drill-down table, filters, grouping, chain roll-up and inspector; Engine: usage series request with normalizers and coverage notes

Scope: in-scope

### Record reasoning in the execution launch tuple

Scope: add `work_executions.reasoning` beside `driver`, `model` and `effort_level`, written once at spawn by `engine/work-db/src/execution_launch_config.rs` from the resolved spawn config, and switch the usage builders' reasoning dimension to prefer it over the task's current value with a `dimension_started` note at the migration. Small, additive; makes the reasoning grouping historical rather than current.

Deliverables:

- Migration and schema assertion; write at spawn; builder preference with a test; the "current setting" label removed from the reasoning dimension for executions that carry the column.

Effort: small

Depends on: Engine: usage rows request, projection, crate, CLI caller and seed fixture

Scope: deferred (future / not a v1 blocker) — the window ships with reasoning labelled as the task's current setting; recording it at spawn is a capture change this project did not ask for, filed here so the decision is visible.
