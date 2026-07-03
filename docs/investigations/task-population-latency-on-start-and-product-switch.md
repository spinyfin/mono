# Investigation: multi-second task-population delay on app start / product switch

**Status:** investigation complete — no code changed. Deliverable is this writeup plus ranked follow-up chores. **R1 (end-to-end per-segment timing) has since shipped and produced live diagnostics; §10 reports the results and supersedes the speculative attribution in §6–§8 with measured numbers.**
**Date:** 2026-07-01 (revised 2026-07-01 — real-data DB validation, see §9; revised 2026-07-03 — live production timing diagnostics, see §10)
**Execution:** `exec_18be5484b40c4f68_123` (original); `exec_18be578c9ec3e7a0_25c` (real-DB-snapshot revision); `exec_18bee836b1025060_33` (live-diagnostics revision)
**Scope:** measure and attribute the wall-clock of populating the kanban board (lanes, nav, counts) on (a) cold app start and (b) product switch, then propose remediations grounded in the measurements.
**Real-data validation:** all DB-dependent numbers in §3/§5 were re-measured against a read-only `.backup` snapshot of the live engine database (§9). **Live validation (§10):** with R1's instrumentation now running in production, the full engine→wire→decode→apply→render path has been measured directly on the live dataset (1,949-item Boss product) instead of reconstructed from a DB snapshot plus structural reasoning about the unmeasured app/engine segments. §10 is the authoritative attribution; read it first, then §3–§9 for how the investigation got there.

---

## TL;DR

**Superseded by live measurement (§10, 2026-07-03):** production diagnostics from R1's instrumentation now show directly, on the real 1,949-item Boss product, that **the engine-side RPC round trip (`request` segment) is 92–99% of wall clock — p50 ~7.1 s** — and that **all client-side cost (decode + apply + render combined) is ~0.6 s**. This confirms and sharpens, rather than contradicts, the structural reasoning below: the DB N+1 this pass measured on a snapshot (§5, ~38 ms) is not what's running on the live engine at live cardinality; something in the engine-side request path costs ~7 s per full-population fetch, scaling super-linearly with item count (182 items → ~0.1 s; 1,949 items → ~7.1 s, i.e. 10.7× the items costs ~70× the time). **Client-side remediations (R4/R5 as originally framed around render cost) are deprioritized — the measured client-side budget is too small to matter.** The engine-side per-item cost is now the unambiguous #1 priority. See §10 for the full data and re-ranked remediations; §6–§9 below are retained as the investigation's original reasoning trail but should be read through the §10 correction.

- Populating the whole board for a product is **one RPC**: `GetWorkTree { product_id }` → one `WorkTree` reply. There is **no app-issued per-task N+1**.
- Inside that one RPC there **is a database N+1**: `get_work_tree` fans out to individual SQLite queries via `collect_task_runtimes`. **The real Boss-product population this loop runs over is 1,908 items (933 tasks incl. revisions + 975 chores), not the 395 "board tasks" the original pass counted** — the RPC's `tasks` query pulls in `kind = 'revision'` alongside `project_task`/`design`/`investigation`, and a separate `chores` query (975 rows) feeds the same loop; the original workload characterization (§3) counted only the 395 project_task/design/investigation rows and missed both. That undercounts the real fan-out by **4.9×** (1,131 → **5,593 queries**, measured on the real snapshot).
- **Despite that 4.9× bigger population, the warm-cache cost only grew from ~25 ms to ~38 ms p50** (real snapshot, stable across reruns) — SQLite's page cache absorbs the extra point lookups within one connection far better than the original per-item extrapolation assumed. Serialize (~18 ms, up from ~2.5 ms — payload is 7.4× bigger), transport (~6 ms, up from <1 ms), and JSON decode (~15 ms proxy, up from ~2 ms) also grew with the bigger real payload. **The entire warm engine→wire→decode path is now ~78–80 ms — still under 100 ms, and still an order of magnitude short of "multi-second," but with a much thinner safety margin than the original ~25–45 ms estimate.** So the warm path still does **not** explain a _multi-second_ delay, but it is no longer a rounding error either — see §9.
- The multi-second symptom is therefore still dominated by the two segments this pass **could not instrument directly**: **(1) cold engine autostart** on cold app start (73 schema migrations + subsystem init at boot), and **(2) the app-side main-thread apply+render of SwiftUI cards** on both flows. Real data sharpens this: **93% of the full 1,908-item population is `done` (318/395 board tasks, 503/538 revisions, 956/975 chores)** — history dominates even more than the original 80% estimate suggested. There is **zero existing per-segment timing** on this path, so today you cannot read the breakdown off any log.
- Two concrete amplifiers make it worse: cold start issues **`GetWorkTree` twice** for the restored product, and the engine runs the blocking SQLite N+1 **on the tokio async thread with a strictly-serial per-connection dispatch loop**, so a slow tree build stalls the other ~7 cold-start requests behind it.
- **Highest-leverage first step is still to add end-to-end timing instrumentation** (R1) — the warm path grew but is still not clearly dominant, so instrumentation remains the only way to confirm where the seconds actually go. Real data does change remediation priority within R3–R6: **payload slimming (R4)'s win grew from −41% to −64%** (real description text is heavier and there's more of it), while **R3's absolute ms savings are modest (~38→17 ms) but its query-count growth (1,131→5,593 as of today) is unbounded** since `done` history is never archived — see §9 for the re-ranking.

---

## 1. Symptom and flows

When the Boss app launches, or when the user switches the selected product, there is a multi-second gap before tasks appear in the kanban lanes, the nav, and the counts. Two flows are in scope:

- **Cold app start** — app process launches, connects to (or spawns) the engine, and populates the initially-selected product's board.
- **Product switch** — engine already running; the user selects a different product and the board repopulates.

The distinction matters: only cold start pays engine-process startup. A product-switch delay must be explained _without_ engine boot, which sharply narrows its likely cause.

---

## 2. Methodology

### 2.1 What I could and could not touch

The live engine database (`~/Library/Application Support/Boss/state.db`) is **engine-owned and gated off** to worker/coordinator sessions — no `sqlite3`/`EXPLAIN` against real data, no reading the file, and that gate is unchanged by this revision. What changed for this revision: a **read-only `.backup` snapshot** of that database (`boss-real.db`, 23.5 MB, taken 2026-07-01 19:18 PDT) was made available at a coordinator-approved path outside the gated directory. This pass copied it into the worker's scratchpad (never opened the original path read-write) and re-ran every DB-dependent measurement against it directly — real schema, real rows, real cardinalities, no reconstruction needed. §9 reports what changed.

The original pass (before this snapshot existed) measured with two sanctioned surfaces, and those numbers are superseded below wherever they diverge:

1. **The `boss` CLI** (talks to the live engine over its socket) to read the **real workload cardinalities** — this undercounted, see §9.1.
2. **A synthetic SQLite database rebuilt from the repo's own schema** (`tools/boss/engine/core/src/work/schema_init.rs`) and seeded to those (undercounted) cardinalities, then driven with the **exact SQL the engine runs** (copied verbatim from the source).

The macOS app and the Rust engine were **not built or run** in this pass (no build/run harness in the worker). App-side and engine-boot costs are therefore still characterized _structurally_ (from the code, corroborated by three independent source-mapping passes) and bounded by reasoning, not timed — the real-DB snapshot has no bearing on those two segments, which remain the leading unmeasured candidates (§6, §8).

### 2.2 Measurement harnesses

Python harnesses (SQLite via the stdlib `sqlite3` module — the same SQLite engine rusqlite links) reproduced under `scratchpad/`:

- `profile_worktree.py` / `profile_scaling.py` / `profile_payload.py` — the original pass's synthetic-DB harnesses (395 tasks, 40 projects, 564 executions, 763 runs, deps). Superseded by the real-DB harnesses below but kept for the batched-vs-N+1 comparison methodology.
- `profile_real_worktree.py` — runs the **exact `get_work_tree` query sequence** (verbatim SQL from `work/workitems.rs` and `work/dispatch_helpers.rs`) against the real snapshot for the Boss product; times the N+1 warm p50/p95; runs `EXPLAIN QUERY PLAN` on every hot query.
- `profile_real_batched.py` — runs a windowed/`GROUP BY` batched alternative (mirrors R3) against the same real population and times it for comparison.
- `profile_real_payload.py` — builds the real `WorkTree` tasks+chores payload from the snapshot and times JSON encode/decode and payload size with/without `description`.

### 2.3 Proxy caveats (direction of error)

- **rusqlite is generally faster per query than Python's `sqlite3`** (no interpreter overhead per step). So the warm DB numbers below are a **conservative upper bound** for the real engine.
- **Swift `JSONSerialization` decode is typically slower than Python's C `json`.** So the decode numbers are a **lower bound** for the app.
- Timings are warm-cache and single-process; they isolate compute cost, not the true-cold-start disk and boot costs (which are exactly what §8 flags as unmeasured).
- The real-snapshot harnesses were re-run 3× each after an initial noisy sample (one run showed 136–401 ms on the N+1 loop, ~5–8× every other run, almost certainly host contention from concurrent shell activity in the worker). Every real-data number reported below is the **stable, reproduced** figure (consistent across reruns to within ~10%), not the outlier.

Numbers below were taken on this machine (Apple Silicon, macOS 15.3 / Darwin 25.3).

---

## 3. Workload characterization (real numbers, from the DB snapshot)

Re-measured directly against `boss-real.db` (the read-only snapshot), which lets this section report exact SQL-level counts instead of `boss` CLI output — and that changes the finding materially: `boss task list` (used originally) only surfaces `project_task`/`design`/`investigation` rows, but `get_work_tree`'s actual DB queries fan out over a **bigger set** that CLI listing doesn't show in one call.

| Metric                                                                          | Value                                                      | Source (SQL against snapshot)                                                                                                                                                                    |
| ------------------------------------------------------------------------------- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Products (active / total)                                                       | 3 / 4 — **unchanged, confirmed**                           | `SELECT status FROM products`                                                                                                                                                                    |
| "Board tasks" on Boss (`project_task`/`design`/`investigation`)                 | **395 — unchanged, confirmed exactly** (331/50/14 by kind) | matches original `boss task list` count precisely                                                                                                                                                |
| — by status (board tasks only)                                                  | done 318, todo 32, blocked 31, in_review 14 (0 active)     | close to original (317/32/31/12/3) — natural task movement between the two measurement dates, not a data-quality issue                                                                           |
| **Revision tasks on Boss (`kind = 'revision'`) — NOT in the original count**    | **538**, 93% done (503/538)                                | `get_work_tree`'s `tasks` query is `kind IN ('project_task','design','investigation','revision')` — revisions were always part of the RPC's fan-out, never part of the original characterization |
| **Chores on Boss (`kind IN ('chore','followup')`) — NOT in the original count** | **975**, 98% done (956/975)                                | separate `chores` query in `get_work_tree`, fed into the same `collect_task_runtimes` N+1 loop and the same `WorkTree` payload as tasks                                                          |
| **Full population `collect_task_runtimes` actually iterates**                   | **1,908** (933 tasks incl. revisions + 975 chores)         | 4.9× the 395 the original pass modeled                                                                                                                                                           |
| Projects on Boss                                                                | 50 (was modeled as 40)                                     | `SELECT COUNT(*) FROM projects WHERE product_id = boss`                                                                                                                                          |
| Executions on Boss (all kinds)                                                  | 4,737 (was modeled as 564)                                 | join `work_executions` → `tasks`                                                                                                                                                                 |
| Runs on Boss (all kinds)                                                        | 3,897 (was modeled as 763)                                 | join `work_runs` → `work_executions` → `tasks`                                                                                                                                                   |
| Dependencies on Boss                                                            | 646                                                        | `collect_product_dependencies` query                                                                                                                                                             |
| Tasks on Flunge (board tasks)                                                   | 41                                                         | same query, `product_id = flunge`                                                                                                                                                                |

The dominant fact is now sharper than originally stated: **the 395 "board tasks" characterization was numerically correct for what it counted, but `get_work_tree` doesn't stop at those 395 rows.** It also fetches every `revision` task (538, mostly historical revision-chain entries) and every `chore`/`followup` (975) into the same N+1 loop and the same wire payload. Across all **1,908** items actually fetched, **93% are `done`** (318 board tasks + 503 revisions + 956 chores = 1,777 of 1,908) — even more history-dominated than the original 80% estimate. Every board load still fetches, serializes, transports, decodes, and (per the app code in §4/§6) at least potentially renders all of it. See §9.1 for why this gap existed and what it changes.

---

## 4. The path, end-to-end

Both flows converge on **one request**:

```
app  ──GetWorkTree{product_id}──▶  engine handler  ──▶  WorkDb::get_work_tree  ──▶  SQLite
app  ◀──────WorkTree{...}────────  serialize (serde_json)  ◀── assemble ◀────────  (N+1 queries)
app: decode (off-main) ─▶ group/sort/@Published apply (MAIN thread) ─▶ SwiftUI render cards (up to ~1,908 items incl. chores — §3, §9)
```

- Protocol: `FrontendRequest::GetWorkTree { product_id }` (`tools/boss/protocol/src/wire.rs:667`); reply `FrontendEvent::WorkTree { product, projects, tasks, chores, task_runtimes, dependencies }` (`wire.rs:1539`).
- Handler: `work_items::handle_get_work_tree` (`tools/boss/engine/core/src/app/work_items.rs:814`), dispatched at `app.rs:2377`.
- DB assembly: `WorkDb::get_work_tree` (`tools/boss/engine/core/src/work/workitems.rs:293`).
- App fetch triggers: `.connected` (`ChatViewModel.swift:1783`), `.productsList` (`:1888–1893`), `selectWorkProduct` (`:942`), `.workInvalidated` (`:1864`).
- Transport: **Unix-domain socket, newline-delimited (`\n`) framing, uncompressed UTF-8 JSON, one frame per message** (`client/src/lib.rs:112`; Swift `EngineClient.swift:344,1236,1283`). No length prefix, no chunking, no compression.
- Decode: **off the main thread** on a private serial queue (`EngineClient.swift:318`), via `JSONSerialization` + manual dictionary walking.
- Apply + render: `@MainActor ChatViewModel.handle(.workTree)` (`ChatViewModel.swift:1898`) replaces the product's buckets wholesale and sorts several times; SwiftUI then renders the lanes.

Key structural properties:

- **Subscriptions are invalidation-only.** `TopicEventPayload::WorkInvalidated { reason, product_id, item_ids }` (`wire.rs:2571`) is a _cache-bust signal, not data_; the app responds with a **full** `GetWorkTree` refetch (`ChatViewModel.swift:1864`). Nothing is streamed or delta'd.
- **Product switch is a full refetch + wholesale replace**, no diffing (`ChatViewModel.swift:928–946`, `:1904–1936`).
- **No git/gh/network in the hot path.** `resolve_task_doc_pointer` is passed `|_| None` for the workspace lookup and its URL helpers are pure string builders (`products_design.rs:705`); `gh` is only used by `create_revision`, not the read path.

---

## 5. Measurements and dominant-cost breakdown

### 5.1 The database N+1 (measured on the real DB snapshot, warm cache)

`get_work_tree` issues a fixed handful of list queries **plus** a per-work-item fan-out:

- `collect_task_runtimes` (`dispatch_helpers.rs:231`) loops over **every task + chore** returned by the two list queries and runs, per item: `query_latest_execution_for_work_item`; then (if the latest row isn't `running`/`waiting_human`) `query_live_execution_for_work_item`; then (if an execution exists) `query_latest_run`. On the real snapshot that's **2-3 queries x 1,908 items** (933 tasks incl. revisions + 975 chores - §3), not the 395 the original pass modeled.
- A second loop resolves `resolve_task_doc_pointer` for each design/investigation (per-task-doc) item - cardinality unchanged from the original estimate (<=64, gated on `task_uses_per_task_doc`; `workitems.rs:363`), contributes a small, roughly-constant addition not separately re-measured here.

**Measured on `boss-real.db` for the real Boss-product population (1,908 items):**

| Path                                                                   | Queries   | warm p50   | warm p95  | (original synthetic-DB estimate, 395 items) |
| ---------------------------------------------------------------------- | --------- | ---------- | --------- | ------------------------------------------- |
| **N+1 (as shipped)** - `collect_task_runtimes`                         | **5,593** | **~38 ms** | ~40 ms    | 1,131 queries / 24.6 ms / 25.3 ms           |
| Batched alternative (window/GROUP BY: "latest exec + run per product") | 1         | ~17 ms     | ~18-25 ms | 7 queries / 6.4 ms / 6.8 ms                 |

The query count grew **4.9x** (1,131 -> 5,593), almost exactly tracking the 4.9x item-count growth (395 -> 1,908) - confirming the N+1 really is linear in item count, as the original pass claimed. But the **warm-cache wall clock only grew 1.5x** (24.6 ms -> ~38 ms), not 4.9x: within a single warm connection, SQLite's B-tree page cache and the OS page cache absorb the extra point lookups far more cheaply than the first 395 did, because many of the extra rows' index pages are already resident by the time the loop reaches them. **This is a real, reproduced number** (stable across 3 reruns after discarding one contention-noise outlier - see §2.3) - it is not an artifact of measuring fewer items than intended.

The batched alternative still wins decisively in relative terms (~38 ms -> ~17 ms, ~2.2x faster) but the **query-count gap is what should worry you going forward**: 5,593 queries today, growing without bound as the Boss product's `done` history keeps accumulating (no archiving exists - §9.2). The original pass's scaling test (execution-history depth vs. task count, synthetic DB) is unaffected by this revision - its conclusion (N+1 cost is bound by item count, not history depth) still holds and wasn't re-run against the snapshot, since it isolates a variable (execution-history depth per item) that the real snapshot can't cleanly vary.

A true cold OS-page-cache pass - where the extra ~4,500 B-tree descents from the population-count gap would hurt the most - remains unmeasured; see §8.

### 5.2 EXPLAIN QUERY PLAN (real DB snapshot)

Re-run against `boss-real.db`. Same query shapes, same index usage, same verdicts as the synthetic DB - the **structural conclusion is unchanged and confirmed at real scale**:

| Query                                | Plan                                                                      | Verdict                                                                                                                 |
| ------------------------------------ | ------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| list tasks for product (933 rows)    | `SEARCH tasks USING INDEX tasks_product_idx` + `TEMP B-TREE FOR ORDER BY` | Index-served on `(product_id, kind, deleted_at)`; the `ORDER BY` sort is a temp B-tree over 933 rows - **still cheap**. |
| list chores for product (975 rows)   | same shape                                                                | Same.                                                                                                                   |
| latest execution per work item       | `SEARCH work_executions USING INDEX work_executions_work_item_idx`        | Index-served; fast **per query** - the problem is the _count_ (5,593 of them), not any single query's plan.             |
| live execution per work item         | same index                                                                | Same.                                                                                                                   |
| latest run per execution             | `SEARCH work_runs USING INDEX work_runs_execution_idx`                    | Same.                                                                                                                   |
| batched alternative (windowed query) | `MATERIALIZE` + `SEARCH ... USING INDEX ...` per CTE, no full scans       | Confirms the batched rewrite stays index-served even at 1,908-item scale - no new index needed for R3.                  |

No large full-table scan, no missing index that matters at this scale even with 4.9x the rows, and **no WAL lock contention** - the read path opens a WAL snapshot (`schema_init.rs:377`, `journal_mode=WAL`, `busy_timeout=5s`), so the engine's writers do not block it. **The bottleneck is still the number of round-trips, not any single slow query - this conclusion survives real-data validation unchanged.**

### 5.3 Payload, serialize, transport, decode

Real full `WorkTree` tasks+chores payload, built from the actual 1,908-row snapshot data (not modeled description sizes):

| Measurement                                  | Real value (1,908 items)             | Original synthetic estimate (395 items) |
| -------------------------------------------- | ------------------------------------ | --------------------------------------- |
| Full payload (with `description`)            | **6,115 KiB** (7.4x bigger)          | 827 KiB                                 |
| Payload without `description`                | 2,196 KiB (**-64%**)                 | 489 KiB (-41%)                          |
| `description` share of payload               | **64%**                              | 47%                                     |
| Serialize (proxy) p50                        | ~18 ms                               | 2.5 ms                                  |
| Decode (proxy - **lower bound**) p50         | ~16 ms                               | 2.2 ms                                  |
| Unix-socket transport of 6,115 KiB @ ~1 GB/s | ~6.3 ms (bandwidth still negligible) | ~0.85 ms                                |

Two things compound here, and it's worth separating them (this is the core of §9.1's answer to the operator's question):

1. **Population size.** Most of the payload growth even with `description` stripped (489 -> 2,196 KiB, everything-but-description) comes from fetching 4.9x more rows (revisions + chores the original pass didn't model), not from bigger text.
2. **Description text weight.** Restricted to just the original 395 board tasks, real mean `description` size is **1,918 bytes** - about **2.2x** the ~856 bytes/item the original 827->489 KiB split implies. Chores are heavier still (mean 2,828 bytes) and revisions lighter (mean 759 bytes, median 37 - most are short revision instructions). **The operator's suspicion was correct**: real markdown volume is meaningfully heavier than the synthetic fixtures modeled, and it's the single biggest per-byte contributor at 64% of payload - but it's compounding with, not replacing, the population-size gap as the dominant driver of the 7.4x payload growth.

### 5.4 The reframe

Summing the **measured warm** segments, real snapshot, real population:

| Segment                                      | Real warm cost                          | Original estimate              |
| -------------------------------------------- | --------------------------------------- | ------------------------------ |
| DB `get_work_tree` (N+1, as shipped)         | ~38 ms                                  | ~15-25 ms                      |
| serialize (proxy)                            | ~18 ms                                  | ~2.5 ms                        |
| transport (bandwidth)                        | ~6 ms                                   | <1 ms                          |
| decode (proxy, >=)                           | ~16 ms                                  | ~2-15 ms                       |
| **Total measured warm engine->wire->decode** | **~78-80 ms** (<= ~160 ms even doubled) | ~25-45 ms (<= ~100 ms doubled) |

**The crux honest finding survives real-data validation, but with a much thinner margin than originally stated: the warm, measurable path is still under 100 ms - still short of "multi-second" - but it grew ~2x (25-45 ms -> ~78-80 ms) once measured against real cardinalities instead of the undercounted synthetic model.** So the perceived multi-second delay is still not explained by the DB N+1, serialization, transport, or decode when caches are warm - but this conclusion no longer has the ~10x headroom the original pass reported; it has closer to 4-5x. If the Boss product's item population keeps growing at its current rate (§9.2), this warm-path floor will keep rising and is worth watching, not dismissing.

---

## 6. Where the seconds actually go (attribution + amplifiers)

Given §5.4, the residual seconds live in segments this pass could not time directly. Ranked by likelihood:

1. **App-side main-thread apply + render (both flows — strongest candidate for product switch).** Decode is off-main, but `handle(.workTree)` runs on `@MainActor` and, in one synchronous burst, evicts+rebuilds the product's buckets and sorts them several times (`ChatViewModel.swift:1903,1930,1932`; `computeVisibleWorkItems` `:696`; `workItems(in:)` `:2681`) — repeated O(n log n) over items — _then_ SwiftUI builds and lays out the lanes. **Real data sharpens this: 318/395 (80.5%) of board-task cards are `done`, and — since chores are first-class rendered items too (§4, `ChatViewModel.swift:696-716`) — 93% of the full 1,908-item population fetched by the same RPC is `done` (318 board tasks + 503 revisions + 956 chores).** If the lanes aren't virtualized, building that many card views synchronously is the most plausible multi-hundred-ms-to-second cost, and it applies to product switch (no engine boot). The app already ships a `MainThreadStallMonitor` that logs stalls >250 ms to `diagnostics/ui-stalls-*.jsonl` — **that log would confirm or refute this immediately on a live run** but was not available here.
2. **Cold engine autostart (cold start only).** On launch, if the socket is unreachable the app spawns the engine (`EngineProcessController`, `ChatViewModel.swift:1450`). Engine boot runs **73 schema migrations** (`schema_init.rs`, most no-ops on an up-to-date DB but still executed) plus subsystem init (pollers, sweeps, host registry, cube heartbeat) before it can serve `GetWorkTree`. Plausibly 1–3 s, entirely on the cold-start path, and invisible to product switch. **Not timed here** — the real-DB snapshot has no bearing on this segment (it is a boot-time cost, not a data-volume cost).
3. **True cold OS-page-cache DB reads (cold start).** The N+1 (now measured at **5,593 queries** against the real snapshot, not 1,131 — §5.1) becomes materially more expensive when the DB pages aren't resident (each point query is a fresh set of B-tree descents from disk). Bounded by reasoning to tens–hundreds of ms for a 23.5 MB DB (the real snapshot's actual size), but the true cold-read cost was still not measurable here (no way to evict the OS page cache without sudo — §8).

Two amplifiers that make both worse and are cheap to confirm/fix:

- **Redundant double `GetWorkTree` on cold start (confirmed by reading the source).** With a product restored from `UserDefaults`, `.connected` fires `sendGetWorkTree(productID)` (`ChatViewModel.swift:1783`) and then `.productsList` fires `sendGetWorkTree(productID)` **again** for the same product (`:1892–1893`, the `else if let productID = currentSelectedProductID` branch). Two full tree builds back-to-back — at real cardinality, that's ~78–80 ms doubled to ~156–160 ms of warm engine→wire→decode cost alone, before render.
- **Blocking SQLite on the async runtime + strictly-serial dispatch.** `get_work_tree` runs synchronously inside the `async fn` handler with **no `spawn_blocking`** (`work_items.rs:825`), and the per-connection loop `.await`s each handler before reading the next request (`app.rs:2256`). So the ~5,593-query tree build (real cardinality) blocks the tokio worker thread and stalls the other ~7 cold-start requests (`ListProducts`, `GetEngineHealth`, `GitHubAuthStatus`, live states, …) queued behind it — and the redundant second `GetWorkTree` doubles that.

### Existing instrumentation: none on this path

There is **no wall-clock timing** on `GetWorkTree`/`ListProducts` at any layer (confirmed across engine + app):

- `engine-trace.jsonl` is an event log with **no span durations**; `get_work_tree` emits **no trace line at all**.
- `ipc_log.rs` logs only the reverse _pane-control_ channel (`SpawnWorkerPane`, …), not `FrontendRequest`s, and carries only an epoch stamp, no `elapsed`.
- Metrics are **counters/gauges only** — no latency histograms.
- **No `#[instrument]`/`info_span!` anywhere**; `Instant::now()` appears only in background loops, never around a handler/query/serialize.
- Swift side: `UISignpost` covers only the Ghostty terminal panes; `designDocTimingLog` covers only the design-doc open path. The task board fetch/decode/render has no `Date()`/signpost timing. (`MainThreadStallMonitor` >250 ms is the one signal that would incidentally catch a render stall.)

You cannot currently read the cold-start/switch breakdown off any log — which is why R1 leads the remediations.

---

## 7. Ranked remediations

**Superseded ranking:** this table reflects the original (DB-snapshot + structural-reasoning) pass, before live production diagnostics existed. §10.4 has the current, live-data-validated ranking — read it first. R1 (below) has since shipped and is what produced the §10 data.

Each is tied to the measurement that justifies it. "Win" is relative to the _measured_ dominant cost; where the dominant cost is unmeasured (render/boot), the remediation is ranked by expected leverage and flagged as gated on R1.

| #      | Remediation                                                                                                                                                                                                                                                                                                                                                                          | Expected win                                                                                                                                                                                                                                                                                                                                                                       | Complexity            | Measured basis                                                                                                                           |
| ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| **R1** | **Add end-to-end per-segment timing.** Engine: `Instant` around `get_work_tree` (total + list-queries + runtime-N+1 + serialize), logged with `duration_ms`. App: `Date()`/`os_signpost` around send→receive→decode→main-thread-apply→first-render in `EngineClient.consumeLines` / `ChatViewModel.handle(.workTree)`; surface existing `MainThreadStallMonitor` hits for this path. | Unblocks correct prioritization; without it the ~78–80 ms-warm engine→wire→decode path (real cardinality) still looks negligible next to "multi-second" but isn't as negligible as first measured.                                                                                                                                                                                 | S                     | §6: zero timing exists on this path; §5.4: warm path still <100 ms real-data-validated, but margin shrank ~2×.                           |
| **R2** | **Kill the redundant double `GetWorkTree` on cold start.** In `.productsList`, only refetch when the selection actually changed; the restored product was already fetched on `.connected`.                                                                                                                                                                                           | Halves cold-start engine board work (and removes one full main-thread apply) — now ~78–80 ms saved per fetch at real cardinality, not ~25–45 ms.                                                                                                                                                                                                                                   | XS                    | §6 amplifier, confirmed at `ChatViewModel.swift:1783` + `:1892–1893`.                                                                    |
| **R3** | **Batch `collect_task_runtimes` (and the doc-pointer loop).** Replace the per-task fan-out with one windowed/`GROUP BY` "latest execution + latest run per work item for this product" query (mirror the existing `attach_ai_reviewing_flag` `IN(...)` pattern).                                                                                                                     | **Real-data-validated: 5,593 → 1 query; ~38 → ~17 ms warm** (was modeled as 1,131→7 / 24.6→6.4 ms). Absolute ms win is smaller in relative terms than modeled (2.2× vs 3.8×), but this remediation now also **caps unbounded query-count growth** — 5,593 queries today will keep climbing as `done` history accumulates (§9.2), which the original 395-item model never surfaced. | S–M                   | §5.1 harness (real snapshot).                                                                                                            |
| **R4** | **Slim the board payload.** Stop serializing `description` (and other card-irrelevant fields) in the `WorkTree` tasks/chores; fetch description lazily when a card opens (the on-demand `loadExecutions`/`loadTranscript` pattern already exists).                                                                                                                                   | **Real-data-validated win grew materially: 6,115 → 2,196 KiB (−64%, was modeled as −41%)**; directly cuts the now-larger ~18 ms serialize + ~16 ms decode + ~6 ms transport costs roughly proportionally. Promoted: this is now a bigger absolute win than R3 at real scale.                                                                                                       | S–M (protocol change) | §5.3 (real snapshot).                                                                                                                    |
| **R5** | **Incrementally populate + virtualize the `done` lane** (and the chores list — §9.2). Render the active lanes (todo/doing/blocked/review) first; lazy-load / paginate `done` and chores (they're history). Confirm the lanes use `LazyVStack`.                                                                                                                                       | Attacks the strongest product-switch candidate — real data strengthens this further: 318/395 = **80.5%** of board-task cards are `done`, and **93%** of the full 1,908-item fetched population is `done` once chores/revisions are counted.                                                                                                                                        | M–H                   | §3 (93% done, full population); §6 #1. Gated on R1 to confirm render dominance.                                                          |
| **R6** | **Move blocking SQLite off the tokio thread (`spawn_blocking`) + add a small read-connection pool.** Stops the tree build stalling the other cold-start requests; pool avoids per-call file-open + PRAGMA.                                                                                                                                                                           | Improves cold-start concurrency (7 requests currently serialize behind the slow one) — the request they serialize behind is now measured at ~78–80 ms real, not ~25–45 ms modeled.                                                                                                                                                                                                 | S–M                   | §6 amplifier (`work_items.rs:825` no `spawn_blocking`; `app.rs:2256` serial; `schema_init.rs:377` fresh connection/PRAGMA per call).     |
| **R7** | **Snapshot-then-delta protocol.** Replace "invalidate → full refetch" with a delta push keyed on the `item_ids` that `WorkInvalidated` already carries; send counts/active lanes first. Correct invalidation only — no stale-serving.                                                                                                                                                | Removes full refetch on every invalidation and every switch — now removes a ~78–80 ms real fetch each time, growing over time (§9.2), not a ~25–45 ms one.                                                                                                                                                                                                                         | H                     | §4 (invalidation-only subscription); §6 #1. Gated on R1.                                                                                 |
| **R8** | **Reduce engine boot cost** (cold start only): gate the 73-migration pass behind the `schema_version` check so an up-to-date DB skips it; keep the engine resident so cold boot is rare.                                                                                                                                                                                             | Attacks cold-start-only seconds.                                                                                                                                                                                                                                                                                                                                                   | M                     | §6 #2. **Gated on R1** — do not touch until boot time is actually measured; unaffected by real-data validation (not a data-volume cost). |

**Explicitly not worth doing** (measured to be non-issues, confirmed at real scale in §5.2): adding an index for the list `ORDER BY` (the temp-B-tree sort is negligible even at 933/975 rows, §5.2); "fixing" WAL lock contention (there is none — readers snapshot, §5.2); compressing the wire (transport bandwidth is ~6 ms even at 6.1 MiB real payload, §5.3 — decode/render, not bytes/s, is the cost).

**Suggested sequencing (updated after real-data validation, §9.3):** R1 → R2 (both cheap, and R1 makes everything else measurable) → then whichever of R3/R4/R5 the R1 data shows dominant. **R3 and R4 are now closer in priority than the original pass suggested** — R4's win grew from −41% to −64% (bigger absolute ms saved today), while R3's absolute ms win is more modest at real scale (~38→17 ms) but addresses an unbounded query-count growth risk (5,593 today, climbing as `done` history accumulates, §9.2) that the 395-item model never surfaced. Do both; they're still safe, low-risk wins regardless of what R1 finds. R6 is a good hygiene fix bundled with R3. R5/R7/R8 are larger and should wait for R1's numbers — real data strengthens R5's premise (93% of the full population is `done`) but doesn't resolve the render-cost question R1 exists to answer.

---

## 8. What was NOT measured (honest baseline)

Updated after real-data validation (§9) — the DB-related gap is **closed**; everything else is unchanged:

- ~~**The live `state.db`.** Engine-owned and gated to worker sessions...~~ **Closed by this revision.** A read-only `.backup` snapshot of the live DB was made available and used directly for every measurement in §3 and §5 — real per-item execution/run counts, real description-size distribution, and real row counts are now **observed, not approximated**. (Soft-deleted-row volume and WAL size specifically: the snapshot has 155 soft-deleted Boss-product rows and is 23.5 MB total; not broken out further as they don't feed the hot path.)
- **True cold OS-page-cache cost.** Still could not evict the OS cache (no sudo), so the disk-read component of cold start is **bounded by reasoning, not timed** — and it is exactly where the now-measured 5,593-query N+1 (up from the originally modeled 1,131) is most expensive. This gap is unaffected by having the real snapshot; it requires evicting the cache, not better data.
- **Engine autostart / boot wall-clock.** The engine was not built or run; the 73-migration + subsystem-init cost is identified structurally but **not timed**. Unaffected by the real-DB snapshot — this is a boot-time cost, not a data-volume cost.
- **App-side render / main-thread apply wall-clock.** The macOS app was not built or run (no harness in the worker). The synchronous group/sort burst + SwiftUI render of cards is identified structurally (three source passes) but **not timed**. Real data raises the stakes here: if all fetched items are rendered (not just board tasks), the app may be building close to 1,908 card views synchronously, not ~400. A live run's `MainThreadStallMonitor` (`diagnostics/ui-stalls-*.jsonl`) would settle this immediately.
- **Absolute engine vs. proxy overhead.** Timings use Python `sqlite3` (DB — rusqlite is faster, so warm DB numbers are a **conservative upper bound**) and Python `json` (decode — Swift `JSONSerialization` is slower, so decode numbers are a **lower bound**). This caveat applies equally to the real-data numbers in §5.
- **Concurrent-writer contention under real dispatcher load.** WAL gives readers a snapshot and the source shows no read-side locking, but this was **not stress-tested** against a live engine mid-dispatch. The snapshot is a static point-in-time `.backup`, not a live connection, so it can't exercise this either.

The single highest-value follow-up (R1) still exists to close the remaining gaps: with per-segment timing on a live run, the true dominant cost becomes readable and R5–R8 can be prioritized against real numbers instead of the structural inference this pass still has to rely on for boot and render.

---

## 9. Real-data validation (2026-07-01 revision)

Prompted by an operator concern: _"the fake tasks it populated don't have the volume of markdown text on them that we have in the real database, so its tests may not be very valid."_ This section re-ran every DB-dependent measurement in §3 and §5 against a read-only `.backup` snapshot of the real engine database (`boss-real.db`, 23.5 MB, taken 2026-07-01 19:18 PDT) instead of the synthetic reconstruction, and reports what changed.

### 9.1 Was the operator right?

**Partly, and the full answer is more interesting than "yes."** Two distinct gaps existed, and description-text weight was the smaller of the two:

1. **Description text volume — confirmed heavier, as suspected.** Restricted to the exact same 395 "board tasks" the original pass modeled, real mean `description` size is 1,918 bytes vs. the ~856 bytes/item the original 827→489 KiB split implies — about **2.2× heavier**. Chores (not modeled at all originally) are heavier still, at 2,828 bytes mean. The operator's intuition was correct.
2. **Population size — a bigger, previously-unidentified gap.** The original pass's workload characterization (§3) used `boss task list --product boss`, which reports 395 rows (`project_task`/`design`/`investigation` only). But `get_work_tree`'s actual SQL — the RPC this whole investigation is about — also pulls every `revision` task (538 more) and every `chore`/`followup` (975 more) into the same `collect_task_runtimes` N+1 loop and the same wire payload. The real population the RPC touches is **1,908 items, 4.9× the 395 modeled** — and this gap exists independent of description-text weight; it would have been wrong even with zero-byte descriptions everywhere.

These compound: the 7.4× payload growth (827 KiB → 6,115 KiB) is roughly 5.3× from population size and a further ~1.4× from heavier per-item text (5.3 × 1.4 ≈ 7.4). Both were real gaps in the original pass's measurement validity; the operator flagged the one that's easier to notice by eyeballing a task description, but the population-count gap was actually the larger contributor to the original numbers being wrong.

### 9.2 A finding the original pass had no way to surface

Because the synthetic DB was hand-seeded to a fixed size, the original pass had no signal that the item population **keeps growing** — `done` history is never archived or paginated (95%+ of every kind fetched by `get_work_tree` is `done`: 318/395 board tasks, 503/538 revisions, 956/975 chores). Every future board load fetches, serializes, and transports the entire history, and the query count (5,593 today) grows linearly with it. This is a forward-looking risk the original 395-item model was structurally unable to reveal, and it strengthens the case for R3 (cap the query count now, before it's 10,000+) and R5 (paginate/virtualize `done` and chores) independent of what R1's instrumentation eventually shows about render cost.

### 9.3 What survived vs. what changed

| Original conclusion                                                            | Real-data verdict                                                                                                                                                                                        |
| ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| No app-issued per-task N+1; single `GetWorkTree` RPC                           | **Survives, unchanged.**                                                                                                                                                                                 |
| Warm engine→wire→decode path is well under 100 ms                              | **Survives, but margin shrank ~2×** (25–45 ms → 78–80 ms). Still short of "multi-second," now by ~4–5×, not ~10×.                                                                                        |
| N+1 queries are individually index-served; no missing index; no WAL contention | **Survives, confirmed at 4.9× the row count** (§5.2).                                                                                                                                                    |
| N+1 cost is bound by item count, not execution-history depth                   | **Survives** — real query-count growth (4.9×) tracked real item-count growth (4.9×) almost exactly.                                                                                                      |
| 80% of board is `done`, historical                                             | **Survives and strengthens** — 80.5% of board tasks, 93% of the full fetched population once chores/revisions count.                                                                                     |
| Workload characterization: 395 tasks on Boss product                           | **Partially wrong** — 395 was correct for `boss task list`'s definition of "task," but `get_work_tree` fetches 1,908 items; this was a scope gap in the original methodology, not a stale-count problem. |
| N+1 costs ~15–25 ms warm; payload is 827 KiB                                   | **Both materially undercounted** — real: ~38 ms warm N+1 (5,593 queries), 6,115 KiB payload. Root cause: the 4.9× population gap above, compounded by ~2.2× heavier real description text.               |
| R3 (batch N+1) expected win: 1,131→7 queries, 24.6→6.4 ms (−74%)               | **Direction confirmed, magnitude revised**: 5,593→1 query, ~38→~17 ms (−55%). Relative win is smaller than modeled, but the query-count-growth risk (§9.2) is a new argument for doing it.               |
| R4 (slim payload) expected win: 827→489 KiB (−41%)                             | **Win grew**: 6,115→2,196 KiB (**−64%**) — real description text is both heavier per-item and a larger fraction of a bigger payload. Promoted relative to R3 for near-term impact.                       |
| R1 (instrumentation) is the highest-leverage first step                        | **Survives, unchanged** — real data narrowed the warm-path/multi-second gap but didn't close it; instrumentation is still the only way to confirm boot vs. render dominance.                             |

No remediation's win reversed direction or dropped below "worth doing." The main practical changes are: **R4's expected win is bigger than modeled**, **R3's absolute win is smaller than modeled but its growth-risk argument is new and real**, and **the overall "warm path is negligible" framing needs updating to "warm path is still short of multi-second but no longer a rounding error, and will keep growing without R3/R4/R5."**

---

## 10. Live diagnostics validation (2026-07-03 revision)

R1 (§7) has shipped: the running Boss build now writes per-segment population timing to `population-timing-*.jsonl` in the application-support diagnostics directory. This section reports the first live capture — 180 events across 15 complete fetch cycles, taken 2026-07-03 ~15:19–15:28 PDT from the live dataset (Boss product = 1,949 items, a second product = 182 items) — and uses it to replace the speculative attribution in §6–§8 with measured numbers. The raw file is coordinator/engine-owned diagnostic state and was not read directly by this pass; the aggregates below were computed from it and handed to this investigation.

### 10.1 Aggregated timings (p50 / max ms, per flow + segment)

| flow                             | segment                | n   | p50      | max      | items |
| -------------------------------- | ---------------------- | --- | -------- | -------- | ----- |
| cold_start                       | request                | 3   | 13,842.7 | 14,470.5 | 1,949 |
| cold_start                       | render                 | 3   | 256.3    | 456.9    | 1,949 |
| cold_start                       | apply                  | 3   | 249.3    | 250.9    | 1,949 |
| cold_start                       | decode                 | 3   | 64.8     | 67.0     | 1,949 |
| cold_start                       | apply.bucket_rebuild   | 3   | 35.5     | 36.4     | 1,949 |
| cold_start                       | apply.sort             | 3   | 3.8      | 4.0      | 1,949 |
| cold_start                       | render.column_build    | 12  | 1.1      | 4.0      | 1,949 |
| cold_start                       | render.compute_visible | 3   | 0.8      | 0.9      | 1,949 |
| invalidation_refetch             | request                | 10  | 7,063.5  | 7,814.3  | 1,949 |
| invalidation_refetch             | render                 | 10  | 268.7    | 364.4    | 1,949 |
| invalidation_refetch             | apply                  | 10  | 247.9    | 252.7    | 1,949 |
| invalidation_refetch             | decode                 | 10  | 62.7     | 65.7     | 1,949 |
| manual_refresh                   | request                | 1   | 7,030.8  | 7,030.8  | 1,949 |
| product_switch (182 items)       | request                | 1   | 91.6     | 91.6     | 182   |
| product_switch (182 items)       | render                 | 1   | 143.0    | 143.0    | 182   |
| invalidation_refetch (182 items) | request                | 3   | 98–162   | 162      | 182   |

Note the honest limits of this sample: n=3 cold starts, n=1 manual refresh, single-day capture (§10.5).

### 10.2 Chronological fetch trace (request segment)

- **Cold start issues `GetWorkTree` twice, concurrently.** `fetch_issued` seq=1 at t=0, seq=2 at t=+85 ms — a duplicate concurrent fetch. seq=1's request completed in 7,478 ms; seq=2, serialized behind it, completed in 14,471 ms. This is exactly the "redundant double `GetWorkTree`" amplifier flagged structurally in §6/R2 — now directly observed: **the duplicate fetch doubles perceived cold-start latency (~7.5 s → ~14.5 s), not because the second fetch is slow on its own, but because it queues behind the first** on the engine's serial per-connection dispatch loop (§6, `app.rs:2256`).
- The same duplicate-fetch pattern reproduced independently a second time: a `manual_refresh`-tagged fetch at 15:24:48.366 was followed 401 ms later by a `cold_start`-tagged fetch on the same product, completing in 7,031 ms and 13,843 ms respectively — same doubling shape, different trigger pairing.
- **Every invalidation triggers a full 1,949-item refetch**, each ~6.9–7.8 s: 10 such refetches were captured, several firing back-to-back roughly 8 s apart (seq 4→5, 6→7, 7→8, 8→9) — i.e., a new refetch was issued almost immediately after the previous one completed, consistent with the "refetch storm" risk flagged in §6/R7 (invalidation-only subscription, no delta, no coalescing).
- The 182-item second product shows the same flows costing 92–162 ms end to end — two orders of magnitude cheaper, at roughly 1/10.7 the item count.

### 10.3 What the live data proves, replacing speculation in §6–§8

1. **The engine-side RPC (`request`: issue → response received) is 92–99% of wall clock.** At 1,949 items: request p50 ≈ 7.06–7.06 s vs. total client-side (decode + apply + render) ≈ 65 + 250 + 270 ≈ **0.6 s**. This directly resolves the question §6/§8 could not answer structurally ("is it render, or is it the request?") — **it is overwhelmingly the request.** The app-side main-thread apply+render candidate ranked #1 in §6 is not supported by live data: it is real (~0.5 s) but roughly 12–14× smaller than the request segment, not the dominant cost.
2. **Item-count scaling is super-linear, not linear.** 182 items → ~0.1 s; 1,949 items → ~7.1 s. 10.7× the items costs **~70×** the time. This is inconsistent with the §5 DB-snapshot N+1 model (which showed near-linear query-count growth and only ~1.5× warm-cache wall-clock growth for a 4.9× item-count increase) and inconsistent with payload/transport cost (decode of the full 1,949-item payload is only 65 ms — bytes-on-the-wire is not the bottleneck). Super-linear scaling of this shape is the signature of **per-row work inside the engine handler or DB layer that itself scales badly** (e.g. an N+1 pattern with per-item overhead that grows with table size — repeated full/partial scans, cache eviction as row count exceeds a fixed buffer, or similar) — not a fixed per-request cost multiplied by a bigger N. **This reopens §5's DB-snapshot N+1 as insufficient explanation**: whatever the live engine binary is actually doing differs from the schema/query-shape model used in §5, or the live table sizes have grown past a threshold the 1,908-row snapshot (§9) didn't cross. The engine-side handler and DB layer need to be profiled directly against the live database (`EXPLAIN QUERY PLAN` against the real live `state.db`, not the July 1 `.backup` snapshot) to find the actual per-item hot loop — this is now the single highest-priority action.
3. **Cold start's concurrent duplicate fetch is confirmed live and costs ~7 s of perceived latency**, reproduced twice in a 180-event, 15-cycle capture — this is not a rare edge case.
4. **Invalidation triggers full 7 s refetches**, with evidence of storms. This doesn't change the per-fetch cost but multiplies how often users pay it.
5. **Minor, unresolved:** `apply` p50 is ~250 ms but its instrumented children (`bucket_rebuild` 36 ms + `sort` 4 ms) sum to only ~40 ms — roughly 210 ms of `apply` is unattributed to any instrumented sub-segment. Noted as an instrumentation gap, not a priority (it's ~3% of total wall clock at live cardinality).

### 10.4 Re-ranked remediations (supersedes §7's ranking)

The §7 table remains as the investigation's original reasoning, but with live data in hand, the ranking changes as follows:

1. **Root-cause and fix the engine-side per-item cost in the list handler (new #1, was R3/R6 in §7).** This is now the dominant, measured cost: ~7 s per full-population fetch, target sub-second. Profile the live handler and DB with `EXPLAIN QUERY PLAN` as originally scoped in R1/R3 — but now knowing definitively _where_ the time goes (the request segment, not render), the next step is to decompose _within_ that segment (handler vs. DB query vs. serialization vs. transport — none of which today's instrumentation separates, §10.5) rather than re-guess from the July 1 snapshot. The §5 snapshot-based N+1 estimate (~38 ms warm) is not what's running live and should not be relied on for magnitude; only the qualitative conclusion (it's a per-row DB pattern) plausibly still holds.
2. **Dedupe concurrent identical fetches on cold start (was R2 in §7, unchanged priority, now with live magnitude).** Confirmed live: this alone roughly halves perceived cold-start latency (~14.5 s → ~7.5 s) for the cost of a client-side guard (`.productsList` should not refetch a product already in flight from `.connected`). Cheap, safe, and does not depend on the engine-side fix landing first.
3. **Invalidation coalescing / delta updates (was R7 in §7, unchanged priority).** Reduces how often the ~7 s unit cost is paid (refetch storms observed live, §10.2) — it does not fix the 7 s cost itself, so it is properly ranked below the engine fix, not above it.
4. **Client-side work (decode/apply/render, was R4/R5 in §7) is explicitly deprioritized.** Measured live cost is ~0.6 s total against a ~7.1 s request — even a 100% reduction in client-side cost would cut total wall clock by well under 10%. Payload slimming (R4) and lane virtualization (R5) may still be worth doing for other reasons (memory, payload size), but they should not be pitched as latency fixes and should not be scheduled ahead of the engine-side fix.

### 10.5 Honest gaps in the live-diagnostics revision

- **Single-day sample.** All 180 events were captured in one ~9-minute window (15:19–15:28 PDT, 2026-07-03). No day-to-day or load-condition variance is captured; p50/max here are not a distribution, they're one session's worth of cycles.
- **n=3 cold starts, n=1 manual refresh.** Both flows have small samples; the max/p50 spread for `cold_start.request` (13.8 s p50 vs. 14.5 s max, n=3) is consistent with the duplicate-fetch mechanism (§10.2) rather than noise, but a larger sample would help confirm the duplicate-fetch pattern is the sole driver of cold-start variance and not one of several contributing factors.
- **`apply`'s ~210 ms unattributed remainder (§10.3 item 5)** is not decomposed by current instrumentation — a gap in the instrumentation, not a mystery worth chasing given its size relative to the request segment.
- **The `request` segment itself is not yet decomposed engine-side.** Today's diagnostics time issue→response-received as one span. It's now confirmed to be the dominant cost, but _within_ that ~7 s, handler-side compute, DB query time, serialization, and transport are still lumped together — exactly the further instrumentation needed to safely start the engine-side fix (#1 above) without guessing.

---

## Appendix A — measurement harnesses

Reproducible standalone Python scripts (no repo build required; stdlib `sqlite3`):

- `profile_worktree.py` — synthetic DB + N+1 vs batched timing + EXPLAIN QUERY PLAN + payload bytes + connection-open cost. (Original pass; superseded by the real-DB harnesses below for all reported numbers.)
- `profile_scaling.py` — N+1 cost vs history depth; cold-SQLite-cache first pass; full path incl. serialize. (Original pass; its conclusion — N+1 cost is bound by item count, not history depth — was not re-run against the real snapshot since it isolates a variable the snapshot can't cleanly vary; see §5.1.)
- `profile_payload.py` — full `WorkTree` payload size and encode/decode timing, with/without `description`. (Original pass; superseded by `profile_real_payload.py`.)
- `profile_real_worktree.py` — **real-data.** Runs the exact `get_work_tree` query sequence against `boss-real.db` for the Boss product; N+1 warm timing; `EXPLAIN QUERY PLAN` on every hot query.
- `profile_real_batched.py` — **real-data.** Windowed/`GROUP BY` batched alternative (mirrors R3) against the same real population, for comparison.
- `profile_real_payload.py` — **real-data.** Builds the real tasks+chores `WorkTree` payload from `boss-real.db` and times JSON encode/decode, with/without `description`.

They reconstruct the schema from (or, for the real-data scripts, connect directly to a copy of) `tools/boss/engine/core/src/work/schema_init.rs`'s schema and run the SQL verbatim from `work/workitems.rs` and `work/dispatch_helpers.rs`. (Kept in the investigation scratchpad, not committed — see the follow-up chore to land the equivalent as an engine bench.)

## Appendix B — key source references

- RPC/protocol: `tools/boss/protocol/src/wire.rs:667` (GetWorkTree), `:1539` (WorkTree reply), `:2571` (WorkInvalidated); `types.rs:2735` (Task, `description` at `:2786`), `:3042` (TaskRuntime).
- Engine handler + dispatch: `tools/boss/engine/core/src/app/work_items.rs:814`; `app.rs:2256` (serial loop), `:2377` (dispatch).
- DB assembly + N+1: `work/workitems.rs:293` (get_work_tree), `:316` (task list SQL), `:363` (doc-pointer loop); `work/dispatch_helpers.rs:231` (collect_task_runtimes), `:239–341` (per-item exec/run queries), `:203` (collect_product_dependencies); `work/revision_helpers.rs:336` (attach_ai_reviewing_flag — the batched contrast); `work/products_design.rs:705` (resolve_task_doc_pointer); `work/schema_init.rs:53` (tasks DDL), `:82` (indexes), `:377` (connect/PRAGMA).
- App fetch/apply/render: `tools/boss/app-macos/Sources/ChatViewModel.swift:1783` / `:1892` (double fetch), `:928` (selectWorkProduct), `:1898` (workTree apply), `:696` (computeVisibleWorkItems), `:2681` (workItems(in:)); `EngineClient.swift:318` (decode queue), `:543` (sendGetWorkTree), `:1236/1283` (framing).
- Transport: `tools/boss/client/src/lib.rs:112`.
- Instrumentation (absence): `engine/core/src/main.rs:58` (engine-trace.jsonl), `ipc_log.rs`, `app/metrics.rs`; Swift `Diagnostics/UISignposts.swift`, `MainThreadStallMonitor`.
