# Performance metrics: read-time series over the primary tables, rendered natively in Swift Charts

- Date: 2026-09-11
- Project: Performance metrics: timeseries capture and interactive SwiftUI charts (`proj_18d42ae93ddc3208_281`)
- Status: design, awaiting review
- Related: `trustworthy-per-work-item-cost-attribution.md` (rate table, token vocabulary), `engine-app-rpc.md` (socket contract), `tools/boss/docs/forensic-surfaces.md`

The central bet of this design is that Boss needs **no new metrics storage** to answer the operator's ask: every series requested (review time, completion time, PRs generated, failed and reaped agents, tokens, cost) is computed at read time from `work_executions`, `work_runs`, and `tasks`, which already hold five months of per-execution facts. The only new capture is three PR-size integers that the review-dispatch path already fetches from GitHub and then throws away, and the only storage-policy change is a narrowing of the execution-retention sweep so it stops deleting the failure history the operator wants to see.

## Verdict

Aggregate on the fly, in the engine, over the existing primary tables, through one generic `GetMetricSeries` RPC (series id, time range, bucket width, group-by dimension, filters). No rollup table, no history table, no snapshotting of the 196 counters. Add three nullable PR-size columns to `work_executions`, populated at review-dispatch time on both review paths with zero extra GitHub spend on the batch path and one attributed `gh pr view` per review on the legacy path. Derive cost through the existing `cost_pricing` seam, and adopt the effective-dated rate table already specified by the cost-attribution design rather than inventing a second one. In the app, the existing Metrics window becomes a two-tab window whose default tab is a new Performance view built on Swift Charts with brush-to-zoom, pinch, a mini-map, and catalog-driven filter chips. The existing counter list moves to the second tab unchanged.

Two findings from verifying the brief against the code change the plan and are called out explicitly below: the retention sweep already deletes the rows the "failed / reaped" series depends on, and cost is more present than the brief states (an in-code rate table, a pure aggregation module, and a `boss cost` CLI all exist and are the right thing to build on).

## Goals

- Give the operator an interactive, high-level view of how Boss, its drivers, and its agents perform over time: review duration, task completion duration, PRs generated, failed and reaped agents, token consumption, and estimated cost.
- Support zooming in and out of time ranges and slicing by driver, model, work-item kind, effort level, repo, product, and PR size.
- Make the new Performance view the primary metrics surface in the macOS app. Keep the existing counter list reachable but demoted.
- Show historical data wherever it already exists, and say honestly where it does not: every chart must distinguish "measured zero" from "not captured yet" at every x position.
- Keep the engine as the system of record. The app sends a query and renders a reply. It never reads `state.db`, never scans the event stream for this feature, and never aggregates.

## Non-goals

- Deleting, hiding, or filtering any of the 196 counters and 13 gauges. They stay, they keep their names, and `bossctl metrics list|show|reset` and `bossctl metrics github` keep their meaning.
- Any external infrastructure: no Prometheus, Grafana, OTLP, external time-series database, or JavaScript bundle. The operator settled the surface.
- Fixing the token-vocabulary and model-attribution defects documented in the cost-attribution design (gross versus net input across drivers, `<synthetic>` labels, reasoning tokens). This design consumes whatever that design's pricing seam produces and labels the result accordingly. It does not re-decide the rate table's shape, provenance rules, or failure policy.
- Populating rate values. No prices appear in this document.
- Per-slot or per-page attribution from the dispatch event stream, and sub-stage timing (release-pane, driver-teardown, cube-release). The event stream stays a forensic surface.
- Alerting or thresholds on any series.

## Background: what exists, verified against the code

The brief's data inventory is measured and trusted. The code-side survey confirms its column spellings and corrects four of its conclusions.

### Primary tables

- `work_executions` (`engine/core/src/work/schema_init.rs:199`): `id`, `work_item_id`, `kind`, `status`, `repo_remote_url`, `created_at`, `started_at`, `finished_at`, plus migration-added `pr_url` (`migrations_a.rs:45`) and `driver`, `model`, `effort_level` (`migrate_work_executions_launch_config`, `migrations_b.rs:2812`). No token columns, no `host_id`. Indexes: `(work_item_id, created_at)` and `(status, priority, created_at)`. Nothing on `kind` or `finished_at`.
- `work_runs` (`schema_init.rs:219`): `execution_id`, `status`, `error_text`, `created_at`, `started_at`, `finished_at`, `host_id`, and the nine cost columns from `migrate_work_runs_cost_columns` (`migrations_a.rs:601`): `model`, `output_tokens`, `input_tokens`, `cache_creation_tokens`, `cache_read_tokens`, `cache_creation_5m_tokens`, `cache_creation_1h_tokens`, `rounds`, `agent_active_ms`. Written by `WorkDb::set_run_cost_snapshot` (`work/run_rows.rs:276`) on every driver hook via `RunCostCapture::capture_and_persist` (`run_cost.rs:368`).
- `tasks`: `kind`, `status`, `pr_url`, `created_at`, `completed_at`, `effort_level`, `reasoning`, `driver`, `product_id`, `project_id`, `repo_remote_url`. The canonical column list is `TASKS_STATUS_CHECK_COLUMNS` (`migrations_c.rs:137`).
- Timestamps are epoch seconds stored as TEXT, exactly as the brief says. Every existing aggregate casts (`CAST(created_at AS INTEGER)` in `execution_retention.rs`, `cost_report_db.rs`).
- Terminal transitions all go through `WorkDb::finish_execution_run` (`work/executions_runs.rs:1791`) and its siblings in the same file (`mark_execution_orphaned`, `fail_execution_start`, `cancel_execution_with`, `fail_pane_parked_execution`). `finished_at` is set only when the status is terminal. There is an un-finalize path, `readopt_inferred_terminal_execution` (`:309`).

### Correction 1: the retention sweep already deletes the failure history

`bossctl executions prune` is not the only pruner. `engine/core/src/execution_retention_sweep.rs` runs `WorkDb::prune_terminal_executions` on a recurring cadence, unconditionally (no feature flag, no setting), deleting `abandoned | failed | orphaned | cancelled` executions older than 14 days beyond the newest five per work item (`execution_retention.rs:73-85`). `work_runs` rows cascade with them, taking their token columns along. The brief observed failed rows back to 2026-04-06 and inferred prune had never run. The more likely reading is that the per-work-item floor kept them. Either way, the "failed / reaped agents" series and the token series are both structurally under-reported beyond 14 days today, and the design has to say what it does about that (see the retention decision below).

### Correction 2: cost is already half built

`grep` of the schemas and config files was correct: no rate card lives in the database or on disk. But one lives in code. `engine/core/src/cost_pricing.rs` has `MODEL_PRICING` (three Anthropic families, four rates each, substring-matched), `price_for_model`, and `estimate_usd`. `engine/core/src/cost_report.rs` is a pure aggregation layer over `CostRunRecord` rows projected by `work/cost_report_db.rs::cost_records_for_window`, and it already carries the honesty vocabulary this design needs: NULL tokens are `runs_unmeasured` and never summed as zero, unknown models set `estimated_usd_partial` and are named in `pricing_gaps`, and `TOKEN_CAPTURE_START_EPOCH_S` (2026-07-27T09:23Z) flags any window that spans the capture boundary. It is exposed as `boss cost task|window|top` (`cli/src/cost_cmds.rs`) over `GetCostWindowReport`, `GetTopCostConsumers`, and `GetWorkItemCostReport` (`protocol/src/wire.rs:816`, `:999`, `:1042`). The cost-attribution design (`trustworthy-per-work-item-cost-attribution.md`, revised 2026-09-02) already specifies the replacement rate table: keyed on `(driver, model_canonical, effective_from)`, version-controlled in the repo, with provenance fields and an enumerated unpriceable policy. This design builds on that seam and does not open a third rate card.

### Correction 3: PR size is on the wire, just not in a column

`enqueue_review_batch` (`engine/core/src/completion/pr_transition.rs:33`) already runs `gh pr view --json baseRefOid,headRefOid,files,additions,deletions` at review-dispatch time and freezes `additions`, `deletions`, and the changed-file list into `ReviewClassification`, which is serialised into `pr_review_batches.classification_json`. That path is behind `review_batch_fanout`, which is **default off** (`engine/feature-flags/src/lib.rs:89`). The default legacy path creates a single `pr_review` execution (`pr_transition.rs:1203`, `work/dispatch_helpers.rs:1599`) with no GitHub call at all. So today: 22 batches have size in JSON, and roughly 4,700 legacy reviews have nothing.

### Correction 4: the app already has a Metrics window, and it already imports Charts

`app-macos/Sources/MetricsViewer.swift` is a Window (`id: "metrics"`, toggled by Cmd+Shift+M from the Window menu, `BossMacApp.swift:72-75`, `:277-294`). It polls `MetricsListLive` every five seconds while open and renders one row per counter with an in-session sparkline drawn with Swift Charts (`MetricSparkline`, `LineMark` + `AreaMark`). The deployment target is macOS 15.0 (`app-macos/BUILD.bazel:196`), which puts every Swift Charts interaction API used below in range. No chart interaction API (`chartXSelection`, `chartOverlay`, `chartScrollableAxes`) is used anywhere in the app yet.

The app also contains one precedent that cuts against the "engine is the system of record" constraint: `ActivityLogView` tails `dispatch-events/current.jsonl` straight from disk through `DispatchEventsTailer` (`DispatchEventsData.swift:177`). That is a bounded live tail of a log file for a forensic viewer, with a best-effort "drop lines that don't decode" contract. It is not aggregation and it is not the system of record, so it is not a precedent for the metrics view reading the database, and this design does not follow it. It is named here so the rejection of app-side reads is checkable against existing practice rather than asserted.

### Engine surfaces to copy, not duplicate

- Aggregate read pattern: `work/github_api_usage_db.rs` (`github_api_usage_by_caller`, and `github_api_usage_window`, which returns the range actually covered so rates are never divided by an imaginary window).
- Project-then-aggregate-in-Rust pattern: `cost_report_db.rs` projects rows into `CostRunRecord`; `cost_report.rs` is pure functions over the slice, unit-tested without a database.
- RPC shape: `FrontendRequest` is `#[serde(tag = "type", rename_all = "snake_case")]` with alphabetically sorted variants (enforced by `wire/sorted_request_variants_test.rs`); handlers live in `engine/core/src/app/<name>.rs` and get one `match` arm in `app.rs`; every new variant must be classified in `engine/worker-policy/src/policy.rs` and `sanitize.rs` or workers are denied and responses scrubbed.
- Swift mirroring is by hand: `EngineClient+Requests.swift` (send), `EngineClient.swift` (type switch), `EngineClient+Parsers.swift` (dictionary unpacking), `Models+Engine.swift` (structs), `EngineEvent.swift` (cases), `ChatViewModel+EventHandling.swift` (state).
- Metrics registry: `engine/metrics-registry` atomics, `engine/metrics/src/persistence.rs` 30-second flush, `MetricsListLive` / `MetricsShowLive` / `MetricsReset` handlers in `app/metrics.rs`.
- Event-stream reader with salvage: `engine/dispatch-reader/src/lib.rs::parse_lines` and `integrity::salvage_damaged_line`. Rotation is 5 files of 100 MB (`engine/dispatch-events/src/lib.rs:1298-1303`), oldest pruned, so the stream is bounded storage, not an archive.

## Alternatives considered

### A. A rollup or history table written as executions finalise

Maintain `metric_buckets(series, bucket_start, group_key, n, sum, ...)` rows, updated in the finalize transaction, and read them directly.

Rejected on the measured volumes and the invalidation surface. The largest table involved is 16k executions plus 15k runs, growing by a few hundred rows a day. `bossctl metrics github` already runs a `GROUP BY` at read time over `github_api_calls`, which at 68k rows in 15 days is four times larger than anything these series scan, and it is instantaneous. A rollup would need invalidation from every mutation that changes a fact after it was first written: `set_run_cost_snapshot` rewrites token columns on every hook until the run ends; `finish_execution_run` and four siblings set terminal status; `readopt_inferred_terminal_execution` un-finalises; the retention sweep deletes. Each is a place a rollup can silently drift, and the standing rule that a cache bypass is never the fix for staleness means each would need a real invalidation signal. Nothing in the ask justifies carrying that for a query that takes single-digit milliseconds. The escape hatch, if a year of growth ever pushes p95 query latency past the budget stated below, is a rollup keyed to the terminal transition in `finish_execution_run`, not a cache in the app.

### B. Turn the 196 counters into the timeseries by snapshotting them

Periodically copy `metrics_counter` and `metrics_gauge` into a history table and chart rates of change.

Rejected because it cannot answer the ask. Counters are lifetime-monotonic totals with no dimensions: `dispatcher.completed` cannot be sliced by model or kind, a `review_pool.*` rate cannot be bucketed by PR size, and nothing in the counter set carries duration or tokens. The per-execution facts the operator wants are already recorded on the primary rows with every dimension attached. `MetricsViewer`'s in-session sparkline is this approach in miniature, and it is fine for what it is: a debug affordance for watching a counter move, which is why it stays on the raw tab untouched.

### C. Fetch PR size from GitHub at chart-render time

Rejected for two reasons, the second of which is decisive on its own. First, quota: a size-bucketed review chart over 4,700 reviews would issue thousands of calls per render against a shared 5,000-per-hour REST bucket that the merge poller already consumes continuously. Second, correctness: the number GitHub returns today for a PR reviewed in June is its size after every subsequent revision and merge, not its size when the reviewer saw it. Only capture at dispatch time produces the fact the chart is claiming to show. The same argument rules out an API backfill of historical reviews: it would be spending roughly an hour of the whole REST budget to record the wrong number.

### D. Build the series over the dispatch event stream instead of the database

The stream carries structured failure stages (`spawn_failed`, `spawn_nack`, `driver_start_timeout`, `cube_lease_auto_reap`) and pool and slot attribution that the database lacks.

Rejected as the primary substrate. The stream has no tokens, its model attribution lives inside `pane_spawned.details.spawn_config` rather than on every record, its retention is five rotated files with the oldest deleted, and every read has to run the salvage path over interleaved lines. It remains the right source for one thing the database cannot give, a stage-level breakdown of why spawns fail, which is kept as a deferred series rather than folded into v1.

### E. Aggregate in the app from a snapshot of rows

Ship the projected rows to the app and let Swift bucket them, giving instant re-bucketing on zoom.

Rejected by the project constraint, and independently by the RPC design: the socket carries newline-delimited JSON, and a 16k-row projection per chart is a megabyte per refresh where a bucketed reply is a few kilobytes. Zoom responsiveness is handled by the engine's query latency budget and client-side debouncing instead.

### F. Extend the existing `boss cost window` shapes rather than adding a generic series RPC

`WindowCostReport` already buckets by kind, model, reasoning, and effort. Adding a time axis to it and reusing it for everything was considered.

Not chosen as the shape, but its internals are reused. The cost report's groupings are fixed and value-specific; the ask needs one contract that carries durations with percentiles, counts, token class sums, and priced USD with coverage flags, under any of nine dimensions. A generic series report does that; the token and cost series inside it are built on `cost_records_for_window` and `cost_pricing` so there is one projection and one pricing seam, not two.

## Chosen approach

### 1. Query architecture: read-time aggregation, project-then-bucket, one latency budget

Each request projects the rows for its window into a flat record type (one per source: execution facts, run cost facts, task facts) using SQL that filters on the window and the requested dimensions, then buckets and aggregates in Rust. That mirrors `cost_report.rs` and keeps percentiles, coverage detection, and group-by logic in pure, unit-testable functions.

Latency budget: 150 ms p95 for any single series over the full five-month range at day buckets, measured on the operator's machine with the live database size. Two additive partial indexes are part of the first engine PR because the series filter on `finished_at` and `kind`, which nothing indexes today: `work_executions(kind, finished_at) WHERE finished_at IS NOT NULL` and `work_runs(created_at)`. Because timestamps are 10-digit epoch-second strings, the SQL compares them as TEXT against 10-digit bounds so the index is usable, and casts only in the projection. If the budget is missed, the fix is a rollup keyed to the terminal transition (alternative A's escape hatch), never a client cache.

Bucket width is chosen by the engine when the client does not name one: hour for ranges up to three days, day up to 200 days, week up to four years, month beyond. The reply always states `bucket_secs`. A reply is capped at 5,000 cells (groups times buckets); when a request would exceed it the engine coarsens the bucket and says so, rather than truncating.

### 2. Where aggregation lives: the RPC contract

Two new read-only requests, app- and CLI-facing, denied to workers in `worker-policy`:

```text
FrontendRequest::GetMetricCatalog {}
  -> FrontendEvent::MetricCatalogResult { catalog: MetricCatalog }

FrontendRequest::GetMetricSeries {
    series: String,                 // catalog id
    since_epoch_s: i64,
    until_epoch_s: i64,             // half-open [since, until)
    bucket: Option<String>,         // "hour" | "day" | "week" | "month"; None = engine picks
    group_by: Option<String>,       // a dimension id from the catalog, or None for one group
    filters: Vec<MetricFilter>,     // { dimension, values } ; AND across dimensions, OR within
}
  -> FrontendEvent::MetricSeriesResult { report: MetricSeriesReport }
```

```text
MetricSeriesReport {
    series, value_kind, bucket_secs, since_epoch_s, until_epoch_s, generated_at_epoch_s,
    groups: Vec<String>,                       // group keys in render order
    buckets: Vec<MetricBucket>,                // one per bucket, ascending
    coverage: SeriesCoverage,
}
MetricBucket { start_epoch_s, cells: Vec<MetricCell> }
MetricCell   { group: String, n: u32, value: MetricValue }
MetricValue  = Count    { n }
             | Duration { p50_ms, p90_ms, mean_ms, max_ms }
             | Tokens   { input, output, cache_write, cache_read }
             | Usd      { estimated: Option<f64>, unpriceable_runs: u32, partial: bool }
             | Points   { points }
SeriesCoverage {
    data_from_epoch_s: Option<i64>,            // earliest bucket with any fact for this series
    dimension_from_epoch_s: Option<i64>,       // earliest fact carrying the group_by dimension
    notes: Vec<CoverageNote>,                  // { kind, epoch_s: Option<i64>, detail }
}
CoverageNote.kind = capture_started | dimension_started | retention_bounded
                  | pricing_flat_rates | pricing_gaps | bucket_coarsened
```

The load-bearing invariant, stated at the level that matters: **a bucket with no fact is absent from the reply; a bucket with facts whose value is zero is present with `n > 0`.** The client never fabricates a zero. This is the same NULL-versus-zero contract `CostMeasurement` documents, lifted to the time axis.

`MetricCatalog` carries, for each series: id, title, `value_kind`, the dimensions it supports, a default group-by, and its coverage. It also carries each dimension's observed values with `first_seen_epoch_s` and counts (so filter chips are built from data, not hardcoded in Swift), and the PR-size bucket edges. The catalog is what lets the app render a series it has never heard of, and it is why adding a series later needs no app change.

Series in v1 (all engine-defined; the app renders by `value_kind`):

| id                   | source                                                                                           | x =                                                      | value                                      | dimensions                                                         |
| -------------------- | ------------------------------------------------------------------------------------------------ | -------------------------------------------------------- | ------------------------------------------ | ------------------------------------------------------------------ |
| `review_duration`    | `work_executions` where `kind = 'pr_review'` and `status = 'completed'`                          | `finished_at`                                            | Duration of `started_at` to `finished_at`  | driver, model, effort_level, repo, product, pr_size                |
| `execution_duration` | `work_executions` where `kind` in the implementation and design kinds and `status = 'completed'` | `finished_at`                                            | Duration                                   | kind, driver, model, effort_level, repo, product                   |
| `task_lead_time`     | `tasks` where `completed_at` is set                                                              | `completed_at`                                           | Duration of `created_at` to `completed_at` | kind, effort_level, reasoning, product, repo                       |
| `execution_outcomes` | `work_executions` with terminal status                                                           | `finished_at`                                            | Count                                      | status, kind, driver, model, effort_level, repo, product           |
| `prs_generated`      | distinct `pr_url` over `work_executions` joined to `tasks`                                       | earliest `finished_at` of an execution carrying that URL | Count                                      | kind, driver, model, repo, product                                 |
| `tokens`             | `work_runs` joined through `work_executions` to `tasks`, via `cost_records_for_window`           | `work_runs.created_at`                                   | Tokens                                     | token_class, model, driver, kind, effort_level, reasoning, product |
| `cost_usd`           | same projection as `tokens`, priced through `cost_pricing`                                       | `work_runs.created_at`                                   | Usd                                        | model, driver, kind, effort_level, reasoning, product              |
| `github_api_points`  | `github_api_calls`                                                                               | `started_at_ms`                                          | Points                                     | caller, api, outcome                                               |

"Reaped" is not a status. The `execution_outcomes` series exposes `status` as a dimension, and the catalog defines a named preset "failed or reaped" as `status in (failed, orphaned, abandoned)` so the app can offer it as one chip without inventing semantics. `work_runs.error_text` stays free text and is not a dimension; clustering it is the deferred event-stream series' job.

`prs_generated` counts distinct URLs, not associations, as the brief requires. A URL is attributed to the bucket of the first terminal execution that carries it, so a PR revised five times counts once, in the bucket where it first appeared.

A thin CLI sibling ships in the same PR as the RPC so the contract has an exercised caller before the app lands: `boss metrics series <id> --since <t> [--until <t>] [--bucket ...] [--group-by ...] [--filter dim=a,b]` and `boss metrics catalog`, with `--json` rendering the wire shape verbatim. These are `boss` verbs (operator CLI) alongside `boss cost`, not `bossctl metrics` verbs, and they change nothing about `bossctl metrics list|show|reset|github`.

### 3. Retention: stop deleting started executions

The failed and reaped series, and the token and cost series, read rows the retention sweep deletes. The sweep's own rationale (`execution_retention.rs` module doc) is bounding the stock of pre-spawn aborts, which never start and carry no duration and no tokens. The decision is to narrow the prunable predicate to executions with `started_at IS NULL`, keeping the age and per-work-item rules as they are. A started execution, whatever its outcome, is a performance fact and is kept. `fail_execution_start` is the path that produces never-started failures, so the noise the sweep exists to bound is still bounded; started-but-failed rows accumulate at a few hundred a month, which the indexes above absorb.

This is a policy change with a recorded rationale on the other side, so it is surfaced as a question for the operator in the attentions manifest rather than decided silently. The alternative, an append-only terminal-outcome ledger written in the finalize transaction and exempt from prune, works but duplicates ten columns per execution to route around a policy whose stated purpose the narrowing already preserves. If the operator prefers to keep the current policy, the series still ship: `SeriesCoverage.notes` carries `retention_bounded { policy_days: 14 }` on every affected series and the app renders the bound. Nothing in the query surface depends on which way this goes.

### 4. PR size capture

Three nullable columns on `work_executions`: `pr_additions INTEGER`, `pr_deletions INTEGER`, `pr_changed_files INTEGER`, set once at creation of a `pr_review` execution and never updated. The execution is the right home because it is what the review-duration series reads, both review paths produce one, and the review's size is a property of what the reviewer was handed, not of the PR's current state.

- Batch path: `enqueue_review_batch` already has `additions`, `deletions`, and the file list in hand; they are threaded through `ReviewBatchCreateInput` onto the three leaf executions in `leaf_member_inputs`. Zero extra GitHub calls.
- Legacy path: one `gh pr view --json additions,deletions,changedFiles` per review dispatch, attributed under a new `callers::REVIEW_DISPATCH` label so its spend shows in `bossctl metrics github` (today the batch path's fetch lands under `completion`, and an unlabelled call would land under `unattributed`). At roughly ten reviews a day this is invisible against the 4,900 calls a day already made. A failed fetch leaves the columns NULL and does not block dispatch.
- One-shot backfill inside the migration: for every `pr_review_batch_members` row whose batch has `classification_json` with `additions` and `deletions`, copy them onto the member's execution. No API calls. Everything else stays NULL and the series' coverage reports `capture_started` at the migration's stamp, recorded in the `metadata` table the way `pr_review_verdicts_since` is.
- Buckets, engine-defined and published in the catalog, on `additions + deletions`: XS up to 50, S up to 200, M up to 1,000, L up to 5,000, XL above. The 200 and 1,000 edges match the review classifier's light and deep thresholds (`engine/pr-review/src/parsing.rs`) so the size chip and the classifier agree on what a big PR is. Executions with NULL size fall in an explicit `unknown` bucket, never dropped.

Adding `additions deletions changedFiles` to the merge poller's `PR_PROBE_FIELDS` would cost zero extra GraphQL nodes and give a size for every tracked PR, but it records size at last poll, not at review, so it does not serve the review chart. It is kept as a deferred entry for a future PR-size distribution view, not a v1 blocker.

### 5. Cost model

Cost is derived at read time from the token facts and never stored. The series calls `cost_pricing::price_for_model` and `estimate_usd` per run, exactly as `boss cost --usd` does, and inherits every honesty property that surface already has: an unpriceable run contributes to `unpriceable_runs` and sets `partial`, never to the sum; the models it could not price are named in a `pricing_gaps` coverage note.

Versioning over time is already decided by the cost-attribution design's Layer 3: an effective-dated, version-controlled rate table keyed on `(driver, model_canonical, effective_from)`, so July tokens are priced at July rates whenever the query runs. This design adopts that decision and adds nothing to it. Until that table lands, the seam prices every run at the current in-code constants; the series reports `pricing_flat_rates` in its coverage notes and the app labels the chart "estimated at today's list rates". Whether shipping the interim flat-rate series is acceptable is the second question in the attentions manifest, because it puts a dollar figure in front of an operator that the cost-attribution design says is priced over a defective vocabulary for Codex and Grok rows. The proposed answer is yes, labelled, because the same figure is already one flag away in `boss cost window --usd` and the chart adds coverage notes that CLI does not have.

The five token classes map as the seam maps them: `input`, `output`, `cache_read` at their rates; `cache_creation_5m_tokens` and `cache_creation_1h_tokens` where both are present, else `cache_creation_tokens` at the flat cache-write rate. That last fallback is the documented disagreement between `cost_pricing.rs` and the Layer 3 policy (which would refuse to price an unsplit write); the series does not take a side, it calls the seam, and the seam's policy is the one that applies.

### 6. Honest range handling

Coverage is computed from data on every request, not from constants baked into the app:

- `data_from_epoch_s` is the earliest fact for the series after filters. For `tokens` and `cost_usd` it is the later of the observed minimum and `TOKEN_CAPTURE_START_EPOCH_S`. For `pr_size`-sliced review duration it is the capture stamp from the migration.
- `dimension_from_epoch_s` is the earliest fact where the group-by dimension is non-NULL. For `driver`, `model`, and `effort_level` on executions this is the `migrate_work_executions_launch_config` era (2026-08), which is how the six-week model-sliced depth against five-month unsliced depth is expressed without anyone hardcoding it.
- Notes carry the reason (`capture_started`, `dimension_started`, `retention_bounded`, `pricing_flat_rates`, `pricing_gaps`, `bucket_coarsened`) and the instant.

The app renders the region of the requested range before `data_from` (or `dimension_from` when grouped) as a shaded band with the note's text, and never draws a line through it. A bucket absent from the reply inside the covered range is a gap in the line, not a zero. When the operator adds a `model` chip to a chart whose range starts in April, the band appears and the legend says "sliced by model: data from 2026-08-xx". This is the mechanism; there is no per-chart special casing.

### 7. The SwiftUI view

**Information architecture.** The existing "Metrics" window keeps its id and its Cmd+Shift+M toggle. Its content becomes a `TabView` with two tabs: **Performance** (default, the new view) and **Raw counters** (the existing `MetricsViewer` body, unchanged, with its five-second `MetricsListLive` poll gated on its tab being selected so the poll does not run behind the charts). The window's default size grows to fit a two-column chart grid. Nothing else about the counter list changes: same rows, same sparkline, same filter field, same 30-second staleness pill.

**Performance tab layout.** A toolbar row with range presets (24h, 7d, 30d, 90d, All) and a custom range, a zoom-history back button, and the global filter chips. Below it a `LazyVGrid` of chart cards, one per catalog series, each with its own group-by picker and a small legend. Below the grid a mini-map: a single full-coverage strip of `execution_outcomes` with the current window drawn as a draggable rectangle. Chips, series, dimension values, and bucket edges all come from `GetMetricCatalog`; the app hardcodes only the renderer per `value_kind`.

**Renderers by value kind.** Duration: `LineMark` for p50 with an `AreaMark` band from p50 to p90 per group. Count: stacked `BarMark` per group. Tokens: stacked `AreaMark` by token class, or by the chosen group. Usd: `BarMark` per bucket with the unpriceable share drawn as a hollow overlay and the total labelled "estimated". Points: `LineMark`. All axes are `Date`-valued so Swift Charts formats ticks by bucket width.

**Zoom and brush, stated concretely because it is the hard part.** Swift Charts has no built-in brush-to-zoom. It is built from three primitives that exist on macOS 15:

- `chartOverlay { proxy in GeometryReader { geo in ... } }` gives a `ChartProxy` and the plot frame. A `DragGesture(minimumDistance: 4)` on a clear `contentShape` over the plot converts `x` to a `Date` with `proxy.value(atX:)` after subtracting the plot frame origin. During the drag a `RectangleMark(xStart:xEnd:)` is drawn in the chart body from the two dates; on release the range becomes the new global window.
- `MagnifyGesture` on the same overlay scales the current window around the anchor location, committed on gesture end.
- `chartXSelection(value: $hovered)` drives a crosshair `RuleMark` and an annotation showing every group's value for the hovered bucket.

Zoom out is a history stack: every committed range pushes the previous one; the toolbar back button and Cmd+[ pop it; double-click on a chart resets to the active preset. Panning is the mini-map's rectangle dragged left or right, plus shift-scroll on the grid. `chartScrollableAxes` is deliberately not used: it owns the horizontal gesture (conflicting with the brush), and it assumes the full domain is loaded client-side, which the windowed query model does not do.

A committed range re-queries every visible card with a 150 ms debounce and a generation counter; replies for a stale generation are dropped. While a card waits it dims rather than clears, so a drag never flashes empty charts. Refresh while visible is a 60-second timer plus the window's `onAppear`; the metrics view does not subscribe to work topics because topic events are per-work-item invalidations, and a chart over 16k rows does not need to react to each one.

**Filters.** Chips are multi-select per dimension and apply to every card that supports that dimension; a card that does not support the dimension shows the chip crossed out on its legend rather than silently ignoring it. Each chip shows its `first_seen` when that is later than the window start, which is the second place the honest-range rule surfaces.

### 8. What happens to the 196 counters

They stay where they are: in the registry, flushed every 30 seconds to `metrics_counter` and `metrics_gauge`, readable by `bossctl metrics list|show|reset` and by the Raw counters tab. None is promoted into the Performance view as a series in v1, because none has history and none carries a dimension; the one aggregate that looks like a promotion, `github_api_points`, comes from `github_api_calls`, not from the `github_api.*` counters. The 13 gauges remain point samples on the raw tab. If a counter is later wanted as a series, the right move is to find the fact rows behind it (as this design does for dispatch outcomes) rather than snapshot the counter.

## Risks / open questions

- **Retention narrowing is a policy change.** Started-but-failed executions accumulate forever under the proposal. At current rates that is a few thousand rows a year, but a future spawn-failure storm that sets `started_at` before failing would not be bounded. The attentions manifest asks the operator to choose between narrowing, a ledger, or accepting a bounded chart.
- **Interim cost figures are priced on a vocabulary the cost design calls defective.** The chart will label them, and the seam swap is invisible to this design when Layer 3 lands, but a labelled wrong number is still a number. The manifest asks whether to ship it before the rate table lands.
- **Model labels are raw strings.** Until the cost design's canonicalisation lands, `<synthetic>` and slug aliases like `opus` appear as their own chip values. The catalog will show them with their counts, which is at least honest.
- **Legacy-path PR-size fetch adds one GitHub call per review.** Attributed and negligible, but it is the one place this design adds spend. It is also the reason the batch-path columns are backfilled from JSON rather than re-fetched.
- **Swift Charts brush precision.** `proxy.value(atX:)` resolves to the nearest bucket start; at hour buckets over three days this is fine, at month buckets over the full range a drag snaps coarsely. The engine re-buckets on the new range, so a coarse first drag is refined by a second one. Verified against the API surface, not yet against a build; the first app PR should attach a capture.
- **`chartXSelection` and a `DragGesture` overlay compete for the same pointer.** The design puts the drag on the overlay and hover on the chart modifier, which is the arrangement the API documents, but the interaction PR must confirm hover still tracks during a brush.
- **The retention question and the PR-size backfill both touch `metadata` stamps and migrations.** They are separate entries with distinct files; only the migration chain tail (`schema_init.rs`) is co-edited, which is an append-only list.
- **A verification claim about the cost-attribution project's status could not be confirmed.** `boss project list` returned no project matching "cost", so whether Layer 3 has been filed as work is unknown to this design; the sequencing above does not depend on it.

## Proposed implementation task breakdown

Breakdown size: 8 entries (6 in-scope, 2 deferred) — the change touches three subsystems (engine query and storage, protocol and CLI, macOS app) with four real seams: the series framework and its first three fact sources, the token and cost series that reuse the cost projection, PR-size capture on two dispatch paths, and the app split into a static-render PR and an interaction PR that co-edit the same card views; plus one bounded retention policy change and two deliberately deferred decisions the design made.

### Engine metric-series query surface with catalog, RPC, and CLI

Scope: Add `GetMetricCatalog` and `GetMetricSeries` to `boss-protocol` (`wire.rs`, `wire/events.rs`, new `types/metric_series.rs` with `bon::Builder` structs), classify both in `worker-policy` as denied to workers, and implement them in a new `engine/core/src/work/metric_series_db.rs` (window-scoped projections for execution facts and task facts) plus a pure `engine/core/src/metric_series.rs` (bucketing, percentiles, group-by, coverage detection, auto bucket width, the 5,000-cell cap) and `engine/core/src/app/metric_series.rs` (handlers, one `app.rs` arm each). Ship the `review_duration`, `execution_duration`, `task_lead_time`, `execution_outcomes` (with the "failed or reaped" preset), and `prs_generated` series, the two partial indexes as an additive migration, and the `boss metrics series` and `boss metrics catalog` CLI verbs with `--json`. Unit-test the pure module without a database; integration-test the projections against a seeded `WorkDb`; assert the p95 latency budget in a test over a synthetic five-month dataset.

Effort hint: large

Dependencies: none

Scope: in-scope

### Narrow execution retention to never-started executions

Scope: Change `prune_terminal_executions_on` in `engine/core/src/work/execution_retention.rs` so the prunable predicate additionally requires `started_at IS NULL`, keep the age and per-work-item floor rules, update the module doc and the `bossctl executions prune` help text to state the new policy, and adjust the retention tests that pin the old predicate in the same PR. Add a test that a started, failed execution older than the cutoff survives a sweep. If the operator answers the attentions question in favour of a bounded chart instead, this entry is replaced by adding the `retention_bounded` coverage note to the affected series in the query surface.

Effort hint: small

Dependencies: none (parallel with the query surface entry; distinct files)

Scope: in-scope

### Token, cost, and GitHub-quota series on the series framework

Scope: Add the `tokens`, `cost_usd`, and `github_api_points` series to the catalog. Build the first two on `cost_report_db::cost_records_for_window` (extending its projection with the 5m/1h split columns and `driver`), price through `cost_pricing::price_for_model` and `estimate_usd`, carry `unpriceable_runs`, `partial`, `pricing_gaps`, and `pricing_flat_rates` into the reply, and anchor coverage on `TOKEN_CAPTURE_START_EPOCH_S`. Build the third on `github_api_usage_db` grouped by caller, api, and outcome. Extend the CLI renderer for the new value kinds. No change to `boss cost`.

Effort hint: medium

Dependencies: Engine metric-series query surface with catalog, RPC, and CLI

Scope: in-scope

### PR-size capture on review executions and the `pr_size` dimension

Scope: Additive migration adding `pr_additions`, `pr_deletions`, `pr_changed_files` to `work_executions` with a one-shot backfill from `pr_review_batches.classification_json` through `pr_review_batch_members`, and a `pr_size_capture_since` stamp in `metadata`. Thread the values already fetched in `enqueue_review_batch` through `ReviewBatchCreateInput` onto the leaf executions; on the legacy path add one `gh pr view --json additions,deletions,changedFiles` under a new `callers::REVIEW_DISPATCH` label before the execution is created in `pr_transition.rs`, tolerating fetch failure as NULL. Register `pr_size` as a dimension on `review_duration` with the engine-defined bucket edges published in the catalog and an explicit `unknown` bucket. Tests cover both paths, the backfill, and the bucket edges.

Effort hint: medium

Dependencies: Engine metric-series query surface with catalog, RPC, and CLI (for the dimension registration; the capture half may be developed in parallel but the PR lands after). Parallel with the token and cost series entry; both append to the catalog, which is incidental overlap.

Scope: in-scope

### App Metrics window: Performance tab, catalog-driven chips, and chart cards

Scope: Add the two requests and their parsers to `EngineClient+Requests.swift`, `EngineClient.swift`, `EngineClient+Parsers.swift`, `Models+Engine.swift`, `EngineEvent.swift`, and `ChatViewModel+EventHandling.swift`. Restructure the `"metrics"` window into a two-tab `TabView` with Performance as the default and the existing `MetricsViewer` as "Raw counters", gating its five-second poll on tab selection. Build the Performance tab: range presets, global filter chips built from the catalog, a `LazyVGrid` of chart cards with per-card group-by, one renderer per `value_kind` (duration band, stacked count bars, stacked token areas, estimated-USD bars with the unpriceable overlay, points line), coverage bands and notes rendered from `SeriesCoverage`, gap-not-zero rendering, the 60-second refresh, and the generation-counter request path. No zoom gestures in this entry. Attach a capture from an isolated instance to the PR.

Effort hint: large

Dependencies: Engine metric-series query surface with catalog, RPC, and CLI; Token, cost, and GitHub-quota series on the series framework

Scope: in-scope

### App Metrics window: brush-to-zoom, pinch, hover, mini-map, and zoom history

Scope: On the chart cards from the previous entry, add the `chartOverlay` drag brush with the live `RectangleMark`, `MagnifyGesture` zoom about the anchor, `chartXSelection` hover crosshair with a per-group annotation, the zoom-history stack with toolbar back and Cmd+[ and double-click reset, the full-coverage mini-map strip with a draggable window rectangle, shift-scroll panning, the 150 ms debounce, and the dim-while-loading state. Confirm hover and brush coexist on the same pointer and attach a capture showing a brushed range and its re-bucketed result. This entry co-edits the card views from the previous entry and must forward-port them preservingly.

Effort hint: medium

Dependencies: App Metrics window: Performance tab, catalog-driven chips, and chart cards

Scope: in-scope

### Dispatch-stage failure series from the event stream

Scope: A `dispatch_failures` series in the catalog built over `dispatch_reader::read_current` with the salvage path, counting `spawn_failed`, `spawn_nack`, `driver_start_timeout`, `spawn_ack_timeout`, `cube_lease_auto_reap`, and `execution_finalized` with a reaped `pane_outcome`, grouped by stage, pool, and the `spawn_config.model` carried on `pane_spawned`; coverage bounded by the oldest surviving rotated segment.

Effort hint: medium

Dependencies: Engine metric-series query surface with catalog, RPC, and CLI

Scope: deferred (future / not a v1 blocker) — the database status series covers the operator's ask; this is the stage-level enrichment the design chose not to put on the file-scan path in v1

### PR size on every tracked PR via the merge-poller probe

Scope: Add `additions`, `deletions`, and `changedFiles` to `PR_PROBE_FIELDS` (zero extra GraphQL nodes), carry them through `PrPollStateInput` onto new `tasks` columns, and expose a `pr_size` dimension on `prs_generated`.

Effort hint: small

Dependencies: PR-size capture on review executions and the `pr_size` dimension

Scope: deferred (future / not a v1 blocker) — records size at last poll rather than at review, so it serves a future PR-size distribution view, not the review-duration chart

**Parallelism summary.**

The query-surface entry and the retention entry start together. After the query surface merges, the token/cost entry and the PR-size entry run in parallel (both append to the catalog; whichever lands second rebases the list). The first app entry follows the query surface and the token/cost entry; the interaction entry follows the first app entry and forward-ports its card views. The two deferred entries are not scheduled.
