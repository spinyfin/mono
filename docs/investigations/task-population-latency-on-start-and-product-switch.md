# Investigation: multi-second task-population delay on app start / product switch

**Status:** RESOLVED — root cause fixed and verified live. See the Status details below for the full resolution summary.

- **Date:** 2026-07-01 (revised 2026-07-01 — real-data DB validation, see §9; revised 2026-07-03 — live production timing diagnostics, see §10; revised 2026-07-04 — engine-side segment breakdown locks down the measured root cause, see §11; revised 2026-07-04 — fix verified live post-PR #1758, see §12)
- **Execution:** `exec_18be5484b40c4f68_123` (original); `exec_18be578c9ec3e7a0_25c` (real-DB-snapshot revision); `exec_18bee836b1025060_33` (live-diagnostics revision); `exec_18befac852e7eb08_87` (root-cause-lock-down revision); `exec_18bf08a26b79d360_b1` (resolution-verification revision)
- **Scope:** measure and attribute the wall-clock of populating the kanban board (lanes, nav, counts) on (a) cold app start and (b) product switch, then propose remediations grounded in the measurements.

**Status details:** §11 locked down the root cause (quadratic client-side read/frame loop in `EngineClient.consumeLines()`); PR #1758 fixed it, shipped in Boss 1.0.162, and §12 confirms the fix on live production timing: `socket_write` p50 for a ~6.28 MB reply dropped ~200× (7,656 ms → 39 ms) and end-to-end population dropped ~29–56× (7.8–10.3 s p50 → ~270–400 ms). No further remediation from this investigation is required; the residual ~270 ms is the known, separately-tracked `db.task_runtimes` N+1 (§11.6 item 3).

**Real-data validation:** all DB-dependent numbers in §3/§5 were re-measured against a read-only `.backup` snapshot of the live engine database (§9). **Live validation (§10):** with R1's instrumentation now running in production, the full engine→wire→decode→apply→render path has been measured directly on the live dataset (1,949-item Boss product) instead of reconstructed from a DB snapshot plus structural reasoning about the unmeasured app/engine segments. **Root cause locked down (§11):** T2165 (PR #1737) shipped engine-side per-segment instrumentation that decomposes §10's black-box `request` segment into decode/DB/assemble/queue_wait/serialize/socket_write; a 2026-07-04 live capture shows **97.8% of wall clock is `socket_write`**, not the DB or render. §11 is now the authoritative attribution; read it first, then §10 for how the request-vs-client split was established, then §3–§9 for the investigation's origin and methodology.

---

## TL;DR

**Superseded by the measured root cause (§11, 2026-07-04):** engine-side segment instrumentation (T2165 / PR #1737) decomposes the §10 `request` black box directly on the 1,998-item large-product population: **97.8% of the ~7.8 s p50 is `socket_write`** — the engine writing its 6.14 MB JSON reply over the local Unix socket at 0.8 MB/s, versus 7–8 MB/s for a 605 KB reply. All eight DB segments combined are 94 ms (1.2%); the DB N+1 this investigation worried about since §5 is a rounding error at live cardinality, not the cause. Write time scales as payload-bytes-squared (10.14× payload → 96.1× time), and reading the actual write-loop code (engine `app.rs`) shows a clean two-`write_all` + `flush` call directly against a raw Unix-socket half — **no per-chunk buffer-copy bug in the engine**. The quadratic mechanism instead traces to the **macOS client's read loop** (`EngineClient.swift`), which re-scans its entire accumulated receive buffer for a newline delimiter on every 64 KB chunk before the single large JSON line completes — an O(n²) client-side scan that throttles how fast the client drains the socket, which is what the engine's `write_all().await` blocks on. See §11 for the full breakdown, code citations, and re-ranked remediations — **fixing the socket-write path (client read loop, or chunked/streamed framing) is now the dominant, ~7.5-of-7.8-second fix; the item-count/render/DB items below are all secondary.** **Superseded by live measurement (§10, 2026-07-03):** production diagnostics from R1's instrumentation showed directly, on the real 1,949-item Boss product, that **the engine-side RPC round trip (`request` segment) is 92–99% of wall clock — p50 ~7.1 s** — and that **all client-side cost (decode + apply + render combined) is ~0.6 s**. §10 correctly located the cost inside `request` but could not decompose it further; §11 does. This confirms and sharpens, rather than contradicts, the structural reasoning below: the DB N+1 this pass measured on a snapshot (§5, ~38 ms) is not what's running on the live engine at live cardinality; something in the engine-side request path costs ~7 s per full-population fetch, scaling super-linearly with item count (182 items → ~0.1 s; 1,949 items → ~7.1 s, i.e. 10.7× the items costs ~70× the time) — **§11 now names that something precisely: `socket_write`, driven by client-side read-loop backpressure, not a DB or render cost.** **Client-side remediations (R4/R5 as originally framed around render cost) are deprioritized — the measured client-side budget is too small to matter.** The engine-side per-item cost hypothesized here is now precisely attributed in §11 to the write/read path, not per-item DB or handler compute. See §11 for the current data and re-ranked remediations (§10 for how the request-vs-client split was established); §6–§9 below are retained as the investigation's original reasoning trail but should be read through the §10/§11 corrections.

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

   **Superseded by §11 (2026-07-04):** the "per-row work inside the engine handler or DB layer" guess above was wrong. Engine-side segment instrumentation (T2165 / PR #1737) shows all DB segments combined cost 94 ms (1.2% of total) at 1,998 items — not the driver of the super-linear scaling. The item-count-squared relationship instead comes from `socket_write` (97.8% of wall clock), whose cost is driven by payload-bytes-squared, not item-count directly (payload bytes and item count are themselves roughly linear in each other, which is why both looked super-linear here). See §11 for the measured breakdown and the code-level mechanism.

3. **Cold start's concurrent duplicate fetch is confirmed live and costs ~7 s of perceived latency**, reproduced twice in a 180-event, 15-cycle capture — this is not a rare edge case.
4. **Invalidation triggers full 7 s refetches**, with evidence of storms. This doesn't change the per-fetch cost but multiplies how often users pay it.
5. **Minor, unresolved:** `apply` p50 is ~250 ms but its instrumented children (`bucket_rebuild` 36 ms + `sort` 4 ms) sum to only ~40 ms — roughly 210 ms of `apply` is unattributed to any instrumented sub-segment. Noted as an instrumentation gap, not a priority (it's ~3% of total wall clock at live cardinality).

### 10.4 Re-ranked remediations (supersedes §7's ranking; itself superseded by §11.6)

**Superseded by §11.6 (2026-07-04):** item 1 below ("root-cause and fix the engine-side per-item cost in the list handler") was the correct priority call but the wrong mechanism — §11's engine-side segment breakdown shows the cost is not in the list handler or DB layer (94 ms combined) but in `socket_write` (97.8%), traced to client-side read-loop backpressure. §11.6 has the corrected ranking; items 2–4 below (dedupe concurrent fetches, invalidation coalescing, deprioritize client render/decode work) are unaffected and still stand.

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

## 11. Root cause locked down: quadratic socket write (2026-07-04 revision)

T2165 (PR #1737, "Instrument engine-side task-population timing end-to-end") shipped `population_timing.rs` and instrumented the writer task in `app.rs` to decompose §10's opaque `request` segment into `decode`, `db.product`, `db.projects`, `db.tasks`, `db.chores`, `db.task_runtimes`, `db.dependencies`, `db.ai_reviewing`, `db.doc_pointers`, `assemble`, `queue_wait`, `serialize`, `socket_write`, and `total`. This section reports the first live capture against that instrumentation — `engine-population-timing-2026-07-04.jsonl`, 25 requests (23 large-product ~1,998-item fetches, 2 small-product 189-item fetches), joined against the app-side population-timing files by the operator — and uses it to replace §10.3 item 2's speculative "per-row engine/DB cost" attribution with the measured mechanism. As with §10, the raw diagnostics files are engine/coordinator-owned runtime state and were not read directly by this pass; the aggregates and raw JSONL lines below were computed from them and handed to this investigation, dated 2026-07-04 local capture.

### 11.1 Measured root cause

The ~7.8 s p50 (17.1 s p95, n=23) population request for the 1,998-item product spends **7.66 s p50 — 97.8% of total — in `socket_write`**, pushing a 6.14 MB response over the local Unix socket at 0.8 MB/s, while the identical pipeline for a 605 KB response sustains 7–8 MB/s. `socket_write` time scales as the **square** of payload bytes (10.14× payload → 96.1× time ≈ 10.14² = 102.8), which is the signature of a per-chunk cost that grows with the amount of data already transferred rather than a fixed per-request cost. It is **not** the database: all eight DB segments combined are 94.4 ms (1.2% of total), and the real N+1 (`db.task_runtimes`: 5,865 queries for 1,998 rows, ~2.9 queries/row at 0.013 ms each = 79 ms) is a rounding error next to `socket_write`. Secondary effect: response writing serializes concurrent requests on the per-connection writer task — `queue_wait` p95 is 8.4 s (outlier requests queued behind a prior slow write hit 15–25 s totals; one request had `queue_wait` 16.3 s).

### 11.2 Per-segment breakdown, large product (~1,998 items, p50/p95 in ms)

| Segment          | p50       | p95        | Notes                                  |
| ---------------- | --------- | ---------- | -------------------------------------- |
| decode           | 0.01      | 0.01       |                                        |
| db.product       | 0.03      | 0.10       | 1 row                                  |
| db.projects      | 0.20      | 0.33       | 50 rows                                |
| db.tasks         | 3.76      | 4.81       | 979 rows                               |
| db.chores        | 10.05     | 16.03      | 1,019 rows                             |
| db.task_runtimes | 78.65     | 86.28      | 1,998 rows, 5,865 queries              |
| db.dependencies  | 0.99      | 1.20       | 669 rows                               |
| assemble         | 0.88      | 1.08       |                                        |
| db.ai_reviewing  | 0.05      | 0.10       |                                        |
| db.doc_pointers  | 0.46      | 0.55       | 64 rows, 77 queries — second N+1       |
| queue_wait       | 59.10     | 8,398      | serialization amplifier, see §11.1     |
| serialize        | 4.29      | 4.97       | payload 6,136,572 bytes                |
| **socket_write** | **7,656** | **8,233**  | **97.8% of total**                     |
| **total**        | **7,832** | **16,376** | n=23: 7,841 / 17,089 across the sample |

Segments sum to total within 0.9–3.2 ms per request — nothing engine-side is unaccounted for.

Verbatim evidence lines from the 2026-07-04 capture (`engine-population-timing-2026-07-04.jsonl`):

```json
{"ts_epoch_ms":1783136943134,"product_id":"prod_18a1c4016e0cef98_1","request_id":"8230A353-D5BB-4E0E-8941-D0DA4E2EAFFE","fetch_seq":1,"segment":"socket_write","duration_ms":7002.977875,"items":1997}
{"ts_epoch_ms":1783136936069,"product_id":"prod_18a1c4016e0cef98_1","request_id":"8230A353-D5BB-4E0E-8941-D0DA4E2EAFFE","fetch_seq":1,"segment":"db.task_runtimes","duration_ms":88.690917,"rows":1997,"db_queries":5864,"items":1997}
```

### 11.3 Scaling (189 items → 1,998 items, 10.6×; payload 604,906 B → 6,134,333 B, 10.14×)

| Segment         | 189 items | 1,998 items | Ratio | Shape                            |
| --------------- | --------- | ----------- | ----- | -------------------------------- |
| all DB combined | 51 ms     | 94 ms       | 1.8×  | sub-linear                       |
| serialize       | 0.44 ms   | 4.29 ms     | 9.7×  | linear                           |
| socket_write    | 79.7 ms   | 7,658 ms    | 96.1× | **quadratic** (≈ payload-ratio²) |
| total           | 194 ms    | 7,841 ms    | ~40×  | dominated by socket_write        |

### 11.4 App↔engine join validation

All 25 engine requests matched an app-side `request` segment (join by timestamp + product_id, all within 101–116 ms). App-observed duration = engine total + a median 42.5 ms constant gap (min 37.1, max 197.9; one 476 ms cold-start outlier is connection setup). Example: engine `fetch_seq` 1 total 7,182.9 ms ↔ app 7,220.0 ms (Δ37.1 ms, flow=`product_switch`). Engine instrumentation fully accounts for the app-observed 7.1–7.9 s from §10 — there is no hidden transport cost outside what §11.2 already itemizes.

### 11.5 Code-level finding: the engine write path is clean; the quadratic mechanism is client-side read backpressure

**Engine write path — instrumentation site and write loop (`tools/boss/engine/core/src/app.rs`):** the per-session writer task pops a queued `FrontendEvent`, serializes it, then writes it directly to the raw Unix-socket half:

```rust
// app.rs:2293-2304 (writer_task, inside handle_frontend_connection)
let write_start = Instant::now();
let mut write_failed = false;
if let Err(err) = write_half.write_all(line.as_bytes()).await {
    ...
} else if let Err(err) = write_half.write_all(b"\n").await {
    ...
} else if let Err(err) = write_half.flush().await {
    ...
}
```

`write_half` is a plain `tokio::net::unix::OwnedWriteHalf` from `stream.into_split()` (`app.rs:2246,2255`; imports at `app.rs:9-10` — `tokio::io::AsyncWriteExt`, `tokio::net::UnixStream`) — there is no custom framing, chunking, or buffering wrapper around it anywhere in `app.rs` (confirmed by grep: no `chunk`, `SNDBUF`, `set_send_buffer_size`, or manual `drain`/`split_at` in the file). `AsyncWriteExt::write_all` in tokio 1.45 (pinned in the workspace root `Cargo.toml`) loops with `buf = &buf[n..]` — a slice re-slice, not a copy — so **each iteration's cost is bounded by the bytes actually written in that iteration, not by the size of the remaining buffer.** The hypothesized "per-chunk O(remaining-buffer) copy/re-slice in the engine's response-write loop" from the initial ask is **refuted for the engine side**: there is no such loop, manual or otherwise. `flush()` on a raw `OwnedWriteHalf` (no `BufWriter` wraps it here) is a no-op poll, contributing negligible cost.

Given the write path itself does O(1)-per-byte work with no engine-side algorithmic bug, the `socket_write` `Instant` window is timing something else: **time blocked waiting for the kernel Unix-domain-socket send buffer to drain**, which happens exactly as fast as the client reads from the other end. Reading the client's read loop (`tools/boss/app-macos/Sources/EngineClient.swift`) finds that mechanism:

```swift
// EngineClient.swift:1331-1332 — 64 KiB per NWConnection receive
private func receiveNext() {
    connection?.receive(minimumIncompleteLength: 1, maximumLength: 64 * 1024) {
        [weak self] data, _, isComplete, error in
        ...
        if let data, !data.isEmpty {
            self.buffer.append(data)        // :1345
            self.consumeLines()             // :1346
        }
        ...
        self.receiveNext()                  // :1356 — re-arm for the next chunk
    }
}

// EngineClient.swift:1360-1363
private func consumeLines() {
    while let newline = buffer.firstIndex(of: 0x0A) {   // :1361 — O(current buffer size)
        let lineData = buffer[..<newline]
        buffer.removeSubrange(...newline)                // :1363
        ...
```

Every socket read is capped at 64 KiB (`:1332`), appended to `buffer` (`:1345`), and then `consumeLines()` scans the **entire accumulated `buffer`** from the start looking for the `0x0A` delimiter (`:1361`). For the single ~6.1 MB `WorkTree` reply — which is one giant newline-terminated JSON line with no embedded `0x0A` (JSON string encoding escapes literal newlines as `\n`), so the delimiter is only found on the very last chunk — this is a full linear scan of the buffer-so-far repeated on **every one of the ~94 chunks** (6.14 MB / 64 KiB) needed to receive the whole reply, before a single line is ever produced. Total scan work is `Σ i·64 KiB` for `i = 1..94` ≈ 293 MB of byte comparisons for one 6.1 MB message — an **O(n²/chunk_size)** cost that is the same "cost grows with remaining/accumulated buffer size" shape hypothesized for the engine, just located on the **client's receive/frame loop**, not the engine's write loop. For the 605 KB small-product reply the same math gives ~10 chunks and ~3.6 MB of scan work — nearly free — which is exactly why small payloads see full 7–8 MB/s throughput while the large one collapses to 0.8 MB/s: **the client falls behind proportionally more as the message grows, and since `receiveNext()` (`:1356`) is only re-armed after `consumeLines()` returns, the client's Unix-socket receive buffer stops draining while that O(buffer) scan runs, applying kernel backpressure straight back to the engine's blocked `write_all().await`.** This client-side quadratic scan — not a database cost, not a render cost, and not an engine-side write bug — is the mechanism behind the measured `socket_write` numbers in §11.1–§11.3.

### 11.6 Re-ranked remediations (supersedes §10.4's ranking)

1. **Fix the O(n²) client read/frame loop — new #1, dominant fix (~7.5 s of the 7.8 s total).** `EngineClient.swift`'s `consumeLines()` (`:1360-1363`) re-scans the whole accumulated buffer for a newline on every 64 KiB chunk. Track a scan cursor (resume `firstIndex(of:)` from the last-scanned offset instead of `buffer`'s start) or switch to a length-prefixed frame so the client never needs to scan for a delimiter across multi-megabyte messages at all. This is a client-side (Swift) fix, not an engine fix — despite `socket_write` being the engine-measured segment name, the code owning the cost is `EngineClient.swift`. Alternatively/additionally, the engine could write in bounded chunks with interleaved yields so a slow client degrades other sessions' `queue_wait` less (§11.1's serialization amplifier) while the client-side fix lands — but the client fix is required to close the ~7.5 s gap itself. Target: sub-second write/receive for a 6 MB reply once client-side scanning is O(1) amortized per chunk.
2. **`queue_wait` serialization resolves as a side effect of (1).** Once the dominant session's write/drain finishes in ~O(n) instead of ~O(n²), the other sessions queued behind it on the per-connection writer task (§11.1, `queue_wait` p95 8.4 s) stop paying multi-second amplified delays. No separate fix needed beyond (1); track it as confirmed-fixed once (1) ships and re-measure.
3. **N+1 cleanups (`db.task_runtimes`, `db.doc_pointers`) — demoted to optional follow-ups worth <100 ms.** R3 (§7) and the `db.doc_pointers` second N+1 (§11.2, 64 rows/77 queries, 0.46–0.55 ms) are real but now measured at 79 ms and well under 1 ms combined against a 7.8 s total — under 1.5% of wall clock even summed. Still worth doing for the unbounded query-count growth argument (§9.2), but they do not move the latency needle Users actually feel. Size: small follow-up chore, unchanged from §7's original R3 sizing.
4. **Payload slimming (R4, §7) — demoted to a secondary, linear win.** Stripping `description` (6.14 MB → ~2.2 MB per §5.3's real-data ratio) cuts `socket_write` roughly proportionally under the confirmed quadratic model — a payload cut of ~64% would cut `socket_write` time by roughly 1 − 0.36² ≈ 87%, which sounds large, but it is strictly worse than fixing the O(n²) scan directly (fix (1) removes the quadratic term itself regardless of payload size, and is what makes payload size stop mattering quadratically at all). Still worth doing as a follow-up chore for memory/bandwidth reasons independent of latency, but should not be pitched as the primary latency fix, and should ship after or alongside (1), not instead of it.

### 11.7 Honest gaps in this revision

- **Quadratic fit rests on two product sizes.** The 96.1× time-for-10.14×-payload observation is a single before/after comparison (189 vs. 1,998 items), not a curve fit across many sizes. The fit is tight (10.14² = 102.8 vs. measured 96.1×, within ~7%), and the independently-derived client-side chunk-scan math (§11.5) predicts the same quadratic shape from first principles, which corroborates the two-point fit — but a third or fourth data point (e.g. a ~600-item and a ~3,000-item product) would make this a real curve rather than an extrapolated line.
- **No per-chunk events.** Today's instrumentation times `socket_write` as one span per request; it does not emit per-syscall or per-chunk timestamps engine-side, so the chunk-size-vs-scan-cost mechanism in §11.5 is derived from reading the client code, not from a chunk-level trace. Instrumenting `NWConnection.receive` chunk arrivals (or the engine's per-`write()` syscall boundaries) would let a future pass confirm the ~94-chunk count and per-chunk scan-cost growth directly instead of computing it from `maximumLength: 64 * 1024` and payload size.
- **Client-side slow-reader backpressure is not 100% excluded by the timing data alone** — the engine-side JSONL only proves `socket_write` correlates with payload size squared; it cannot, by itself, distinguish "client scans quadratically" from some other client-side or kernel-level slowdown. What resolves this is the code read in §11.5: the 8× throughput difference between the 605 KB and 6.14 MB replies on the _same_ client/socket path is exactly what an O(n²) client scan predicts (small payloads pay negligible quadratic overhead; large ones pay a lot), and it is inconsistent with a fixed per-message or fixed-bandwidth kernel/transport cost, which would show the _same_ throughput regardless of size. The client code is the stronger, corroborating half of this evidence, not the timing data alone.
- **`db.task_runtimes` query texts (~2.9/row) are not labelled in the schema** — §5.1's N+1 characterization (`query_latest_execution_for_work_item` / `query_live_execution_for_work_item` / `query_latest_run`) is assumed to still be the shape of the 5,865 queries observed live, but this revision did not re-verify that assumption against the live schema; it is inherited from §5/§9 and is not load-bearing for §11's conclusion (DB cost is 1.2% of total regardless of its internal shape).

---

## 12. Resolution — verified fixed by PR #1758 (2026-07-04 revision)

§11.6 item 1 called for fixing the O(n²) client read/frame loop in `EngineClient.consumeLines()` as the dominant fix. **PR #1758, "app-macos: fix O(n²) buffer rescan in EngineClient.consumeLines()"** (merge commit `83c7ed6ce783a75cd4437fabb0d684abce090d29`, merged 2026-07-04 06:05:49Z / 2026-07-03 11:05pm PDT), did exactly that, and this section confirms — with live production timing on both sides of the fix — that it resolved the problem this investigation exists to explain.

### 12.1 Build verification

The running app was confirmed to be on a build containing the fix: `/Applications/Boss.app`'s `Info.plist` reports `CFBundleVersion`/`CFBundleShortVersionString`/`BossFullVersion` all `1.0.162`, and the app/engine restarted onto this build at 2026-07-04 01:01:44 PDT (08:01:44 UTC) — the pre/post cutoff used below. GitHub release `boss-v1.0.162` (tag commit `5b74a226636c256a8da4afa4f22f5cafcdc2067f`, published 2026-07-04 07:34:37Z) is confirmed ahead of the PR #1758 merge commit (`git compare` reports `status: ahead`, `behind_by: 0`), and `EngineClient.swift` at that tag contains the fix — the `unscannedPrefixLength` scan-cursor field and its use sites (a resumable scan cursor replacing the from-scratch `buffer.firstIndex(of: 0x0A)` scan §11.5 identified) are present at lines 351, 1370, 1375, and 1380.

### 12.2 Pre vs. post comparison (engine-side instrumentation, same payload class as §11)

Engine-side segment timing (`engine-population-timing-2026-07-04.jsonl`, the same instrumentation from T2165/PR #1737 used in §11) captured a continuous window straddling the 1:01:44am PDT restart, for the same large product (~2,038–2,040 items, ~6.28 MB payload — the same payload class as §11's 6.14 MB baseline). Pre-fix window: 2026-07-03 8:49pm–2026-07-04 1:01:38am PDT (n=374). Post-fix window: 1:01:46am–1:21:18am PDT (n=79).

| Metric                 | §11 baseline (n=23)       | Pre-fix full window (n=374) | Post-fix (n=79)       |
| ---------------------- | ------------------------- | --------------------------- | --------------------- |
| total p50 / max        | 7,832 / 16,376 ms         | 15,113 / 193,481 ms         | **271 / 5,137 ms**    |
| socket_write p50 / max | 7,656 ms (97.8% of total) | 8,539 / 92,734 ms           | **39 / 2,366 ms**     |
| queue_wait p50 / max   | 59 / 8,398 ms             | 735 / 100,645 ms            | **81 / 926 ms**       |
| db.task_runtimes p50   | 79 ms                     | 100 ms                      | 84 ms (unchanged N+1) |
| serialize p50          | 4.3 ms                    | 5 ms                        | 5 ms                  |
| socket throughput      | 0.8 MB/s                  | ~0.7 MB/s                   | **~160 MB/s (p50)**   |

`socket_write` improved ~219× (p50, full pre-fix window vs. post-fix; ~196× vs. the §11 baseline); end-to-end total improved ~29–56×. The pre-fix full-window p50 (15.1 s) is higher than §11's clean n=23 capture (7.8 s) because it includes `queue_wait` pile-ups (max total 193 s, consistent with §11.1's serialization-amplifier finding) — the fix eliminates both the base cost and the pile-up, since the pile-up was itself downstream of the slow write (§11.6 item 2).

App-side confirmation (`population-timing-2026-07-04.jsonl`, request segment, items > 1,500): pre-fix p50 10,282 ms (n=704, min 7,019) → post-fix p50 398 ms (n=89). Post-fix per-flow p50s: `product_switch` ~210 ms (n=2), `cold_start` 452 ms (first-ever fetch 1,280 ms, n=11), `invalidation_refetch` 385 ms (n=70), `manual_refresh` 474 ms (n=9).

### 12.3 Representative samples

First post-fix large-product request (1:01:46am PDT), `request_id F5C64D4F-8B34-45DA-9452-F5116452D906`, `fetch_seq` 1:

```json
{"segment":"serialize","duration_ms":4.113292,"payload_bytes":6282204,"items":2038}
{"segment":"socket_write","duration_ms":33.493083999999996,"items":2038}
{"segment":"total","duration_ms":341.841375,"items":2038}
```

`socket_write` for the 6.28 MB payload: 33.5 ms ≈ 187 MB/s, vs. §11's 7,656 ms ≈ 0.8 MB/s for the same payload class.

Median post-fix request (`fetch_seq` 38, 1:03:29am PDT): `socket_write` 89.6 ms, total 272.7 ms, payload 6,282,295 B.

Worst post-fix request (`fetch_seq` 77, 1:19:45am PDT), captured during CPU-saturating concurrent bazel test load (`engine_lib_test` + `clippy` started 1:17am): `db.task_runtimes` 1,499.8 ms, `socket_write` 2,366.0 ms, total 5,136.5 ms — the inflation is uniform across every segment while payload size stayed constant, the signature of machine-wide contention, **not** a recurrence of the quadratic mechanism. Even this worst post-fix total (5.1 s) is below the pre-fix _minimum_ app-side total (7.0 s).

### 12.4 Conclusion

**Definitive yes: PR #1758 fixed the problem this investigation set out to explain.** At the 1:01:44am PDT build boundary — visible within a single continuous log file, ruling out any measurement-methodology change — `socket_write` for the same ~6.28 MB payload collapsed from p50 8.5 s (97%+ of wall clock) to 39 ms, ~200×, exactly where §11.5's client-backpressure mechanism predicted it would if the O(n²) scan were replaced with an O(1)-amortized cursor. End-to-end task population dropped from ~7.8–10.3 s p50 to ~270–400 ms. The `queue_wait` amplification (§11.6 item 2) resolved as a side effect, as predicted. The residual ~270 ms is now dominated by the known, separately-tracked `db.task_runtimes` N+1 (~84 ms, ~5,980 queries/request, §11.6 item 3) plus noise — consistent with §11.6's follow-up ranking, which correctly predicted this would be the next-largest remaining cost once the dominant fix landed.

### 12.5 Caveats

1. The post-fix window is one app session, 79 large-product engine samples over ~19.5 minutes — ample to establish a ~200× signal, but a single-session sample nonetheless.
2. Late-window samples (after ~1:07am) rose to 0.5–5.1 s totals under concurrent bazel load (§12.3); the uniform per-segment inflation there is machine contention, not a regression of the fixed mechanism.
3. Both `cold_start` and `product_switch` flows were exercised post-fix, giving some flow diversity despite the single-session caveat above.
4. Version-to-commit mapping is via the `boss-v1.0.162` release tag (the bundle carries no embedded commit hash), corroborated by the install/restart time (1:01am PDT) postdating the release publish time (12:34am PDT) and by behavior flipping exactly at the restart boundary rather than gradually.

This closes the investigation. §11.6's remaining items (N+1 cleanup, payload slimming) stand as optional, low-priority follow-up chores — not blockers, and not expected to move perceived latency meaningfully given §12.2's residuals.

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
- Instrumentation (absence, original pass): `engine/core/src/main.rs:58` (engine-trace.jsonl), `ipc_log.rs`, `app/metrics.rs`; Swift `Diagnostics/UISignposts.swift`, `MainThreadStallMonitor`.
- Instrumentation (§11, T2165 / PR #1737): `tools/boss/engine/core/src/population_timing.rs` (segment definitions, `PopulationTrace`); writer-task timing sites `app.rs:2281` (serialize start), `:2293-2304` (write_start, the three `write_all`/`flush` calls, `SOCKET_WRITE`/`TOTAL` record + flush).
- Root-cause code (§11.5): engine write loop `app.rs:2246,2255` (`stream.into_split()`), `:2293-2304` (raw `OwnedWriteHalf::write_all`/`flush`, no chunking wrapper); client read/frame loop `tools/boss/app-macos/Sources/EngineClient.swift:1331-1332` (`receiveNext`, 64 KiB `NWConnection.receive`), `:1345-1346` (`buffer.append` + `consumeLines`), `:1360-1363` (`consumeLines` — O(buffer-size) `firstIndex(of: 0x0A)` scan + `removeSubrange`).
