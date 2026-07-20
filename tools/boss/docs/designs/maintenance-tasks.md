# Boss: Automations (scheduled maintenance work)

- **Status:** shipped (v1) and in production. This doc was revised on 2026-07-20, after the project completed, to describe what was actually built — including decisions that changed during implementation and one post-ship reversal.
- **Shipped via:** [#921](https://github.com/spinyfin/mono/pull/921) schema + protocol · [#966](https://github.com/spinyfin/mono/pull/966) engine CRUD/RPC · [#1042](https://github.com/spinyfin/mono/pull/1042) CLI · [#1043](https://github.com/spinyfin/mono/pull/1043) second pool + routing · [#1070](https://github.com/spinyfin/mono/pull/1070) scheduler + occurrence math · [#1077](https://github.com/spinyfin/mono/pull/1077) triage + outcome detection · [#1025](https://github.com/spinyfin/mono/pull/1025) Automations tab + schedule editor · [#1068](https://github.com/spinyfin/mono/pull/1068) pool switcher + backlog exclusion.
- **Notable post-v1 revisions:** [#1098](https://github.com/spinyfin/mono/pull/1098) fixed the app's create-automation wire shape; [#1280](https://github.com/spinyfin/mono/pull/1280) reversed kanban exclusion (automation tasks now shown with a badge); the automation pool grew 3 → 6 → 8 and gained spillover/preemption into interactive slots (2026-07).

## Problem

Boss's work model assumes a human (or the Boss coordinator) decides _what_ to do and _when_. Every task is filed by hand, dispatched off the kanban, and the deliverable is a merged PR. That is the right shape for feature work, but it leaves a whole class of work unserved: the recurring, low-stakes housekeeping that nobody wants to remember to file. "Fix clippy warnings." "Look for duplicated code and extract a helper if it makes sense." "Bump the dependencies that have a clean changelog." This work is _valuable_ but _episodic_ — most days there is nothing to do, and on the days there is, it should not jump the queue ahead of the human's real priorities.

Today the only way to get this is to remember to file a chore, periodically, forever. That is exactly the kind of standing instruction a machine should hold.

This doc describes **Automations**: a standing, triggered instruction that periodically asks "is there a concrete maintenance task to do right now?" and, if so, spawns a normal task to do it. Automations live _outside_ the normal backlog — they are created and managed from a top-level **Automations** tab, and the tasks they produce run in a dedicated agent pool (launched at 3 slots, since grown to 8 — see Pool model) so they don't contend with interactive work. The work they spawn is otherwise an ordinary task: it lands in a worker pane, produces a PR, and trampolines to GitHub review like everything else. Only its _origin_ (an automation) and its _pool_ (automations) differ.

## Goals

- A first-class **automation** entity: a standing instruction with a **trigger**, a **product** (and optional repo), a **standing-instruction prompt**, and an **open-task cap**. _(Shipped as designed.)_
- Initially one trigger type — a cron-like **schedule** — with a schema that is **open to other trigger types later** (event-driven, manual-only) without a migration to the core shape. _(Shipped as designed; still schedule-only.)_
- A **two-phase execution model**: phase-1 _triage_ decides whether concrete work exists right now and is allowed to **skip** the occurrence; phase-2 _execute_ spawns a normal task and runs it to a PR. _(Shipped as designed.)_
- A **dedicated pool of agents**, distinct from the main worker pool, with an Agents-tab affordance to switch between pools. _(Shipped; the fixed sizes and strict isolation the v1 design specified did not survive contact with real demand — see Pool model.)_
- **Robust scheduling**: catch up after a missed fire (laptop was closed) unless the occurrence is stale, and **retry** rather than drop an occurrence when execution is transiently impossible (VPN down, remote unreachable). _(Shipped, with a reformulated staleness rule and simpler retry mechanics than designed — see Scheduling semantics.)_
- A per-automation **open-task limit** enforced at fire time so pending changes can't pile up. _(Shipped as designed, default 1.)_
- Automation-produced tasks surfaced under the Automations tab. _(The original goal — exclude them from the normal backlog/kanban entirely — shipped and was then deliberately reversed in #1280: they now appear on the kanban with an automation badge. See App UI.)_
- CLI verbs (`boss automation …`) and a SwiftUI **Automations** tab, both thin clients over the engine, which owns scheduling, pool accounting, and reconciliation. _(Shipped as designed; engine ownership held completely.)_

## Non-goals

- **Trigger types other than `schedule`.** The schema accepts them (tagged kind + payload), but only the cron variant is implemented. Event-driven triggers (e.g. "on every merge to main") remain deferred.
- **Automations that span products or repos.** An automation belongs to exactly one product and targets exactly one repo per fire, mirroring the one-product-per-work-item rule in `work-taxonomy.md`. _(Held.)_
- **Multi-task fan-out per fire.** A single triage run produces **at most one** task, enforced by the triage prompt and a transactional cap re-check at task creation. _(Held.)_
- **Auto-merge of automation PRs.** Produced tasks trampoline to GitHub review and a human merges, exactly like every other task. _(Held.)_
- **Pre-emption / dynamic pool sizing.** _This v1 non-goal was reversed post-ship._ The design fixed the automations pool at 3 and forbade pre-emption; in production, automation demand regularly exceeded the pool, and the answer was growth (3 → 6 → 8 slots, main pool 8 → 16) plus **spillover with preemption**: automation work can spill into idle interactive ("Lower Decks") slots and be preempted there by mainline work (`dispatch_spillover.rs`, priority mainline > review > spilled automation). The two-pool structure remains; strict isolation does not.
- **A general job scheduler.** This is not cron-as-a-service; the only thing an automation can do is run a triage agent against a standing instruction. _(Held.)_

## Alternatives considered

### A1 — External scheduler (system `cron` / launchd) instead of an in-engine tick

Register each automation's cron expression with the OS scheduler; on fire, the OS runs `boss automation run <id>`.

Rejected. The engine is the only component that knows the open-task count, the automations-pool capacity, and whether the machine can currently reach the git remote — an OS-level cron firing blind would either ignore the open-task limit or have to re-implement the engine's accounting over the CLI. More decisively, Boss is a laptop app that is _frequently asleep_: launchd's catch-up semantics for missed wakeups are coarse and not configurable per the staleness rule this design requires. **Chosen and shipped: an in-engine scheduler.** (It launched as a fixed 30-second `spawn_loop` tick per the original design, and was later made event-driven: the loop now sleeps until the earliest `next_due_at`, clamped to an hour, and is kicked by a `Notify` whenever a mutation changes the schedule.)

### A2 — Reuse the main worker pool with a priority/quota instead of a second pool

Keep one `WorkerPool`, tag automation executions low-priority, and reserve (or cap) some slots for them.

Rejected for v1, and the second pool shipped: `WorkerPool::new_automation(size)` is the same proven primitive instantiated again, and per-pool exhaustion means a full automations pool defers automation work without touching main-pool throughput. **Postscript:** the argument this section originally made — that quota-inside-one-pool invites starvation pressure and pre-emption — turned out to be prescient but not decisive. Under real load the strict two-pool split was itself too rigid, and the shipped system now layers exactly the kind of cross-pool accounting this alternative anticipated (spillover of automation work into interactive slots, with preemption) on top of the two-pool structure. The second pool was still the right foundation: routing, accounting, and the UI all key off it.

### A3 — One hidden "Maintenance" project owns all automation tasks (the brief's open question 1)

The brief sketched a single hidden project owning every automation-produced task, with backlog exclusion keyed on project membership.

Rejected, and the rejection held. A project is the wrong primitive here: the open-task limit is **per-automation**, so even with a shared project we would still need a per-task `source_automation_id` to count correctly — the project adds nothing to accounting. **Shipped: provenance via a `tasks.source_automation_id` FK; no synthetic project.** One nuance the original reasoning got wrong: it assumed the FK would also drive backlog _exclusion_, and that exclusion was reversed post-ship (see App UI) — the FK now drives routing, accounting, provenance links, and a kanban badge instead.

## Chosen approach (as built)

The engine gained an `automations` table, a per-fire `automation_runs` history table, an `automation_short_id_sequences` allocator table, a `source_automation_id` provenance column on `tasks`, a second worker pool, and a scheduler loop. The CLI gained a `boss automation` noun; the app gained an Automations tab and an Agents-tab pool switcher. Everything downstream of "a task was produced" is unchanged except its kanban presentation (badge, not exclusion).

### Data model

#### `automations` table

Shipped exactly as the following DDL (schema version 12 → 13, `migrations_b.rs`):

```sql
CREATE TABLE IF NOT EXISTS automations (
    id                  TEXT PRIMARY KEY,             -- auto_<ts>_<n>
    short_id            INTEGER,                      -- per-product A-namespace (A1, A2…)
    product_id          TEXT NOT NULL REFERENCES products(id),
    name                TEXT NOT NULL,                -- display label
    repo_remote_url     TEXT,                         -- explicit target repo; NULL → product primary
    trigger_kind        TEXT NOT NULL,                -- 'schedule' (extensible discriminator)
    trigger_config      TEXT NOT NULL,                -- JSON payload, shape depends on trigger_kind
    standing_instruction TEXT NOT NULL,               -- the prompt
    open_task_limit     INTEGER NOT NULL DEFAULT 1,   -- per-automation open cap
    catch_up_window_secs INTEGER,                     -- override of engine default (see Scheduling)
    enabled             INTEGER NOT NULL DEFAULT 1,
    created_via         TEXT NOT NULL DEFAULT 'unknown',
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    -- bookkeeping, updated by the scheduler
    last_fired_at       TEXT,
    last_outcome        TEXT,                          -- mirrors latest automation_runs.outcome
    next_due_at         TEXT                           -- epoch-seconds string, UTC
);

CREATE UNIQUE INDEX IF NOT EXISTS automations_product_short_id_idx
    ON automations(product_id, short_id) WHERE short_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS automations_due_idx
    ON automations(enabled, next_due_at);
```

One deliberate deviation from the original draft: `next_due_at` and `automation_runs.scheduled_for` are stored as **epoch-seconds strings**, not RFC3339, for consistency with the rest of the schema (`created_at`, `dispatch_not_before`).

**Trigger representation — tagged `kind` + JSON payload.** As designed, mirroring Product's external-tracker pattern. In protocol code the trigger is a serde-tagged enum with a single variant so far:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTrigger {
    Schedule { cron: String, timezone: String },   // timezone = IANA name
}
```

`trigger_kind` is the persisted discriminator; `trigger_config` stores **only the variant body** — the `kind` field is stripped from the JSON on write and re-injected on read (`automation_trigger_to_db`/`_from_db`), so the discriminator lives in exactly one column.

The `Automation` struct (17 fields) uses `#[derive(bon::Builder)]` with `#[builder(on(String, into))]` per repo convention, as do the `CreateAutomationInput` and `AutomationPatch` types that #921 added alongside it. The DB mappers (`map_automation`, `map_automation_run`) are explicit struct literals per the mapper rule, and normalize empty-string columns to `None`. One known limitation of the patch shape: `AutomationPatch`'s optional fields are `Option<T>`, so an override like `catch_up_window_secs` or `repo_remote_url` **cannot be cleared back to NULL** via update (that would need `Option<Option<T>>`); the limitation is flagged in a code comment and remains open.

**No write-time trigger validation.** `create_automation`/`update_automation` accept any cron/timezone string; a malformed cron is stored and only rejected lazily at scheduler tick (warn-and-skip, counted as a config error). This is a known gap — the original design promised CLI-side validation "by the same crate the engine uses", which never materialized at any layer (see CLI surface).

#### `automation_runs` table (run history)

Shipped exactly as designed:

```sql
CREATE TABLE IF NOT EXISTS automation_runs (
    id                  TEXT PRIMARY KEY,
    automation_id       TEXT NOT NULL REFERENCES automations(id),
    scheduled_for       TEXT NOT NULL,                -- the cron occurrence this run satisfies (epoch secs, UTC)
    started_at          TEXT NOT NULL,
    finished_at         TEXT,
    triage_execution_id TEXT,                          -- the phase-1 work_execution
    outcome             TEXT NOT NULL,
    produced_task_id    TEXT REFERENCES tasks(id),     -- set iff outcome = 'produced_task'
    detail              TEXT                           -- skip reason / failure detail (free text)
);

CREATE INDEX IF NOT EXISTS automation_runs_by_automation_idx
    ON automation_runs(automation_id, scheduled_for);
```

`outcome` values as built:

- `produced_task` — triage created a task (`produced_task_id` set); phase 2 is underway.
- `skipped` — triage ran and decided nothing actionable exists right now, **or** the scheduler recorded a stale missed occurrence for observability.
- `suppressed_at_limit` — the fire was due but the open-task count was already at the cap, so no triage ran.
- `failed_will_retry` — the pessimistic default written at fire time and the state for transient failures. _As-built caveat:_ an occurrence abandoned because its catch-up window elapsed mid-retry is also left in this state (detail `"stale: catch-up window elapsed before retry"`) even though it will never be retried — the designed `failed_gave_up` transition for that path was never implemented (known gap).
- `failed_gave_up` — emitted only when the coordinator's pre-start retry machinery permanently fails a triage execution (and on retry-creation failure), not by the scheduler.
- `pool_throttled`, `triage_running` — post-v1 additions the original design didn't anticipate, surfacing pool-pressure and in-flight states to the UI.

#### Provenance: `tasks.source_automation_id`

```sql
ALTER TABLE tasks ADD COLUMN source_automation_id TEXT REFERENCES automations(id);
CREATE INDEX IF NOT EXISTS tasks_source_automation_idx
    ON tasks(source_automation_id, status) WHERE source_automation_id IS NOT NULL;
```

A non-null `source_automation_id`:

1. **Links a produced task back to its automation** — `boss automation tasks <id>` and per-run task links in the app.
2. **Routes the task's execution to the automations pool** — the dispatcher resolves it with a per-dispatch `SELECT` (see Pool model).
3. **Is the denominator for the open-task limit** — `COUNT(*) WHERE source_automation_id = ? AND status IN ('todo','ready','doing','in_review','blocked') AND deleted_at IS NULL`.
4. **Badges the task on the kanban.** The original fourth role — driving backlog/kanban _exclusion_ — shipped and was reversed in #1280; see App UI.

Produced tasks are inserted as **chores** (`kind = 'chore'`, `project_id = NULL`, `created_via = 'engine_auto'`, `autostart = true`, `force_duplicate = true` to bypass the recent-duplicate guard). The original design said `kind = 'task'`; implementation landed on the chore path (`insert_chore_in_tx`) since these are product-level housekeeping items structurally identical to chores — the design's own analogy, taken literally. There is still no `kind = 'maintenance'`: provenance, not kind, is the discriminating axis.

Deletion semantics (decided during implementation, #966): `delete_automation` is a hard delete that cascades to `automation_runs` in the same transaction; produced tasks are intentionally **orphaned** — they keep their `source_automation_id` and live out their normal lifecycle.

#### Short-id namespace: a new `A` prefix

Automations get their own per-product `A` namespace (`A1`, `A2`, …). The original plan to reuse the existing `short_id_sequences` table "with no schema change" was wrong — that table holds a single per-product counter shared by `T`/`P`, so A-numbers would have interleaved with task numbers. #921 added a dedicated `automation_short_id_sequences` table (`product_id` PK, `next_value`) with a parallel `allocate_automation_short_id()`. Automations are not in `resolve_friendly_work_item_id`; `A<n>` resolution is **CLI-side** (`resolve_automation`: `ListAutomations` + match on `short_id`), which is why every selector-taking CLI verb needs product context (see CLI surface). There is no engine-side `A<n>` resolver.

### Repo selection

As designed: an optional explicit `repo_remote_url` field, authoritative for the cube lease, with the standing instruction as documentation only. If `repo_remote_url` is null, the product's primary repo is used. The produced task inherits the repo via the existing `tasks.repo_remote_url` per-task override.

### Two-phase execution

#### Phase 1 — Triage

When the scheduler fires an automation (due, enabled, under cap), it creates a `work_execution` of kind `automation_triage` (`EXECUTION_KIND_AUTOMATION_TRIAGE`, now a typed `ExecutionKind::AutomationTriage` in `boss-protocol`), **bound to the automation, not to a task**: `work_item_id` is the automation's id, and the coordinator synthesizes an in-memory `WorkItem::Chore` (`synthetic_triage_work_item`) to satisfy spawn plumbing — a deliberate dodge of adding a `WorkItem::Automation` enum variant, which would have rippled through ~50 exhaustive matches. The `automation_runs` row is opened with `outcome = failed_will_retry` as a pessimistic default, so a crash mid-triage leaves a retryable record.

The triage worker is spawned into a cube workspace like any worker, but its prompt is a **triage preamble** rendered by `render_triage_preamble` (`automation_triage.rs`) instead of the normal execution prompt. Key contract points as shipped:

- The create command embeds the **canonical `auto_…` id**, not `A<n>` — `boss task create --automation auto_… --name "<title>" --description "<what>"` — so the agent's call resolves without a `--product` flag. (The original sketch's `--autostart` flag doesn't exist; autostart is hard-coded server-side.)
- Hard guardrails: do not do the work yourself; create at most one task (a second `boss task create --automation` in one run is rejected transactionally); end with **exactly one** decision marker — `automation: task <id>` or `automation: skip — <reason>`. Zero or multiple markers is an inconclusive run, not a skip.
- The preamble has since grown substantially in response to field failures (open question 3's exact worry): an "already tracked" section listing open sibling tasks, a "single-shot mandate — no sub-agents, no deferral" section, and keep-it-lightweight guidance.

**Outcome detection.** The design named an `AutomationTriageDetector` struct mirroring `PrDetector`; what shipped follows `completion.rs`'s method-per-kind shape instead: a `finalize_automation_triage` branch taken on Stop when `kind == automation_triage`, plus a `StopOutcome::AutomationTriage` variant. The marker parser (`parse_triage_decision`) is stricter and looser than the design in instructive ways: it scans **every line** of the final assistant message (not just the last line), enforces **exactly one** marker (a new `Ambiguous` state refuses to guess between two), matches the `automation:` prefix case-insensitively, and accepts em-dash, hyphen, or colon as the skip separator. A `task` marker is verified against the DB — the named task must exist with this automation's provenance — before `produced_task` is recorded.

Post-v1, **marker recovery** was added for inconclusive runs (a class of real incidents, e.g. the "at limit 3/3" wedge): if a run ends with no usable marker but an open task with this automation's provenance exists, it is recorded as `produced_task`; if the final message plainly concluded there was nothing to do, it is recorded as `skipped`.

The triage agent creating the task itself (rather than the engine reading a manifest) shipped as designed: the CLI's `boss task create --automation` maps to a dedicated `CreateAutomationTask` RPC whose handler runs one immediate transaction — cap re-check against `open_task_limit`, chore insert, provenance stamp — so even a misbehaving agent can't exceed the limit.

#### Phase 2 — Execute

The produced task is an ordinary chore row with `source_automation_id` set. The engine requests an execution for it via the normal path, and the dispatcher routes it to the automations pool because the provenance column is non-null. From there the lifecycle is identical to any task: Doing → worker opens a PR → `PrDetector` flips it to `in_review` → human reviews/merges on GitHub → `done`. Since #1280, the main kanban **does** see the task — badged as automation work — because the Automations tab shows run history rather than a full produced-task lifecycle view (see App UI).

### Pool model (as built)

A second `WorkerPool` instance lives on the **`ExecutionCoordinator`** (not `ServerState` as originally sketched — the coordinator owns claim/release; `app.rs` constructs the pool from config and injects it via `set_automation_pool()`, which also gives tests a seam). It is built with `WorkerPool::new_automation(size)`, which namespaces slot ids with an `auto-worker-` prefix — an unplanned but load-bearing design element: the release path only has a worker-id string, so the prefix is what routes releases to the right pool (`pool_for_worker_id`) without a DB round-trip. Size comes from `BOSS_AUTOMATION_POOL_SIZE`, clamped to `MAX_AUTOMATION_POOL_SIZE` — **3 at launch, raised to 6 and then 8 (2026-07-15) as automation demand regularly exceeded the pool**. The main pool likewise grew 8 → 16 (two UI pages, "Bridge Crew" and "Lower Decks"), and a third **review pool** (8, `review-` prefix) was later cloned from the same pattern.

Routing as shipped:

```rust
fn execution_targets_automation_pool(&self, execution: &WorkExecution) -> bool {
    if execution.kind == EXECUTION_KIND_AUTOMATION_TRIAGE { return true; }
    matches!(self.work_db.source_automation_id_for_work_item(&execution.work_item_id), Ok(Some(_)))
}
```

The original sketch read `work_item.source_automation_id` from an in-memory work item the drain loop doesn't actually hold, so the shipped predicate does a single `SELECT` per dispatch. (Known wart: a DB _error_ here silently routes to the main pool.)

Per-pool exhaustion required more than the "single branch" the design promised: the old one-at-a-time `drain_ready_queue` early-returned on the first full pool, which would have let a full automations pool block main dispatch. It was rewritten to batch-fetch ready executions per pass with independent per-pool exhaustion flags; executions for a full pool stay `ready` and are retried on the next kick. `DrainOutcome` remains a binary enum (callers can't tell which pool stalled; the unconditional re-kick on release makes that acceptable).

**Post-v1 spillover.** Strict isolation was relaxed once the pool cap kept binding: `dispatch_spillover.rs` lets automation work spill into idle Lower-Decks interactive slots, where mainline work can preempt it (priority mainline > review > spilled automation). Spilled automation holds `worker-N` ids, so the worker-id-prefix-as-pool-proxy had to be patched with `attributed_pool_label()` for accounting.

The pools draw cube workspaces from the same cube pool (workspaces are repo-scoped and fungible); only slot/pane accounting is separate.

### Scheduling semantics (as built)

The scheduler lives in `automation_scheduler.rs` around a pure `run_one_pass(work_db, now_epoch, dispatcher)` — "now" is injected, so occurrence math is deterministically testable. It launched as the designed 30-second `spawn_loop` tick and fires immediately on boot (so an occurrence that elapsed while the engine was down catches up without waiting an interval); the fixed tick was later replaced by an event-driven sleep until the earliest `next_due_at` (clamped to 3600s, with a 5s bootstrap poll and a `Notify` kick from mutation handlers). Each pass, for each enabled automation with `trigger_kind = 'schedule'`:

1. **Compute occurrences.** Open question 7 asked "`croner` or `cron`?" — the answer was **neither**: occurrence math is a hand-rolled five-field cron parser (now the `boss-engine-automation-schedule` crate) plus `chrono-tz`. Rationale: purity (no DB, no async, no wall-clock reads) and full control of DST resolution so gap/fold behavior exactly matches this doc. The parser supports `*`, values, ranges, lists, steps, dow 0–7 with 7→0, and the Vixie dom/dow union rule; no month/day names. An unsatisfiable cron yields nothing within a 5-year scan horizon.
2. **Open-task-limit gate (at fire time).** Count open produced tasks (`todo|ready|doing|in_review|blocked`, not soft-deleted). If count ≥ `open_task_limit`: don't fire; record a `suppressed_at_limit` run and advance `next_due_at` to the following occurrence — the open-question-8 "advance" choice, so a capped automation doesn't stampede the instant a task merges. (Refinement: if no following occurrence exists, hold rather than lose the slot.)
3. **Due check + catch-up after a miss.** The design's staleness rule — skip if `following - now <= catch_up_window` — was **reformulated during implementation** because it is degenerate for cron periods shorter than the window: an every-5-minutes job always has its next fire within 15 minutes and would have skipped every occurrence. The shipped rule tests the miss itself: an occurrence is **stale** when `now - occurrence > catch_up_window`; stale misses are recorded as `skipped` runs (detail explains the lateness) and the schedule advances. A sleep backlog is **collapsed to the single most recent occurrence** (bounded at 10,000 scanned occurrences per pass) — one catch-up fire after a week's vacation, not a stampede. `catch_up_window` defaults to an engine constant of **15 minutes**, overridable per automation via `catch_up_window_secs` (though no CLI/app surface can currently set the override — known gap).
4. **Dispatch + transient retry.** Firing goes through a `TriageDispatcher` seam (`EngineTriageDispatcher` in production). The design's bespoke exponential backoff ("1, 2, 4, 8 … min capped at the catch-up window") was **not built**; what shipped is simpler reuse: pre-start failures (cube lease, remote unreachable) flow through the coordinator's generic machinery — fixed retry delays of 5s/15s/45s, then permanent failure finalizes the run as `failed_gave_up`. Dispatcher-level transient failures hold `next_due_at` (preserving `scheduled_for` so the same occurrence retries) and re-attempt every scheduler pass until the catch-up window elapses, at which point the occurrence is abandoned — currently mislabeled `failed_will_retry` rather than the designed `failed_gave_up` (known gap).

**Transient inability vs genuine phase-1 skip — the key distinction, as built:**

| Signal                                                                  | Meaning                     | Recorded as                                                                                                                                                            |
| ----------------------------------------------------------------------- | --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pre-start failure (lease/remote/VPN) — worker never produced a decision | Can't execute right now     | `failed_will_retry` → retried (5s/15s/45s), then `failed_gave_up`                                                                                                      |
| Worker ran, ended with `automation: skip — …`                           | Agent decided nothing to do | `skipped` → advance schedule, no retry                                                                                                                                 |
| Worker ran, ended with `automation: task <id>` (verified in DB)         | Found work                  | `produced_task` → phase 2                                                                                                                                              |
| Worker ran, emitted zero or multiple markers                            | Inconclusive                | Marker recovery first; else `failed_will_retry` — but the schedule already advanced at fire time, so the effective retry is the next cron occurrence, not the same one |

The discriminator remains **whether the worker reached a decision marker**; the marker-recovery pass narrows the inconclusive bucket that in the original design would have burned retries.

#### Timezone / DST handling

Shipped as designed: the IANA timezone name is stored alongside the cron, occurrences are computed in that zone, and `(automation_id, scheduled_for)` is the dedupe key — `record_automation_run_and_advance` is a transactional upsert on that pair, so a given occurrence fires at most once regardless of clock weirdness.

- **Spring-forward gap**: advance minute-by-minute to the first valid instant (bounded at 240 minutes) — the job runs once, slightly later.
- **Fall-back overlap**: fire on the **earliest** mapping of the ambiguous wall-clock time only.

### CLI surface (`boss automation`)

All ten designed verbs shipped (`create list show update enable disable delete run runs tasks`), mirroring `boss task`/`boss project` conventions (`--json`/`--no-input` are global flags rendered via `print_entity`). Deviations and refinements vs the original table:

- **Every selector-taking verb accepts `--product`**, because `A<n>` is a per-product namespace resolved client-side. When exactly one product exists it is auto-selected, so `--product` is usually omittable; selectors accept `A<n>`, `a<n>`, a bare integer, or the canonical `auto_…` id.
- `create` flags: `--product --name --instruction --schedule --timezone --repo --open-task-limit --disabled`; missing required values are prompted interactively unless `--no-input`. **`--timezone` defaults to `UTC`** (the app defaults to the system zone — a small inconsistency).
- Presets compile as: `weekday-2pm` → `0 14 * * 1-5`, `nightly` → `0 2 * * *`, `weekly-mon-am` → `0 9 * * 1`, `hourly` → `0 * * * *` (case-insensitive).
- **Raw-cron validation is shallow**: five whitespace-separated fields of `[0-9A-Za-z*/,-]` only. The design's promise — "validated by the same crate the engine uses, so the CLI rejects garbage before it reaches the DB" — is unfulfilled: `99 99 * * *` passes the CLI, is stored, and is only rejected per-tick by the scheduler. The schedule crate even exports a `validate_schedule()` doc-commented for CLI use that has zero callers. Known gap at every layer.
- `run [--force]` shipped in #1042 as a stub (the engine returned "not yet implemented") and became real in #1077: manual fire respecting the cap unless `--force`, recording a run with `scheduled_for = now` and leaving `next_due_at` undisturbed.
- `update` can patch name/instruction/schedule/timezone/repo/open-task-limit (partial trigger patches merge with stored values; `--repo ""` clears). It cannot toggle `enabled` (dedicated verbs) and **`catch_up_window_secs` has no CLI surface at all**.

### App UI (as built)

- **Automations tab** (#1025): `NavigationMode.automations` + a `NavigationSplitView` — sidebar list, detail pane, empty states. Rows show the `A<n>` id, name, enabled status dot, human-readable schedule, color-coded last outcome, and `open/limit` count (via a `GetAutomationOpenTaskCount` RPC the design didn't specify); next-due time and the enabled toggle live in the detail pane rather than the row. The detail pane has schedule/status/instruction/settings sections, an enable toggle, an Edit sheet, and Delete-with-confirmation. Run history was added post-#1025 once a list RPC existed ("Recent Runs" section, per-run produced-task `T…` links that reveal the task on the kanban). Ship note: #1025's create call disagreed with the engine's `#[serde(flatten)]` wire shape and creation was broken at merge until #1098 — the price of item 7's "can start against mocked RPC" plan meaning it was never integration-tested against the real engine.
- **Schedule editor**: presets ("Every weekday at 2pm", "Every night at midnight", "Weekly on Monday at 9am", "Every hour", "Custom…") compiled to cron with reverse cron→preset mapping on edit; a raw-cron escape hatch (validation is a field-count check only); a timezone picker defaulting to the system zone; the compiled cron shown read-only. Plus name, instruction, open-task-limit stepper (1–10), start-enabled toggle, optional repo field. (The edit sheet collects but currently **discards** repo and enabled changes — known gap.)
- **Agents-tab pool switcher** (#1068): shipped as the designed segmented "Main (8)" ↔ "Automations (3)" control; now a four-way picker — Bridge Crew / Lower Decks / Automations / Reviewers — as the pools grew. The designed "worker states tagged by pool" subscription never existed; pool identity is conveyed by **disjoint slot-id ranges** (main 1–16, automations 17–24, reviewers 25–32), with dynamic pool config pushed to the app via `EnginePoolConfig` at session registration. Functionally equivalent, mechanically different.
- **Backlog/kanban exclusion — shipped, then reversed.** #1068 shipped the client-side filter (`computeVisibleWorkItems()` dropped tasks with non-null `sourceAutomationId`); the promised server-side work-tree exclusion was never built. #1280 then deliberately **removed** the client filter: because the Automations tab surfaces run history but not a produced-task lifecycle list, excluded `in_review` tasks had no visible surface anywhere — a task waiting on human PR review was invisible. The shipped end-state is **inclusion with provenance**: automation tasks appear on the kanban with a purple `wand.and.stars` badge and an "Automation" detail row. The design's assumption that "the Automations tab shows this lifecycle; the main kanban never sees the task" is the single largest thing this project got wrong.

### Engine ownership

Confirmed and held throughout: the engine owns the scheduling tick, occurrence computation, open-task-limit enforcement, pool accounting, triage dispatch, outcome detection, and run-history writes. The app and CLI are thin clients. No scheduling logic lives in the app.

### Migration (as shipped)

- Schema version 12 → 13: `CREATE TABLE automations`, `CREATE TABLE automation_runs`, `CREATE TABLE automation_short_id_sequences`, `ALTER TABLE tasks ADD COLUMN source_automation_id` (+ partial index). The original claim that the `A` namespace needed "no schema change" was wrong (see Short-id namespace).
- No change to `work_executions` — `automation_triage` is just a new kind string, and `dispatch_not_before`/`pre_start_failure_count` already existed, exactly as predicted.

### Implementation (as shipped)

The eight planned PR-sized tasks all landed, in the planned dependency order, with scope shifting at the edges:

1. **Schema + protocol types** — #921. Added the unplanned `automation_short_id_sequences` table and the `CreateAutomationInput`/`AutomationPatch` types; deferred cron-crate choice entirely (resolved in #1070 as "neither").
2. **Engine CRUD + RPC** — #966. Landed in a new `work/automations.rs` submodule rather than `work.rs`; added `GetAutomationOpenTaskCount`; defined an `AutomationRunResult` event that was never wired to a producer (superseded by the later `AutomationRunsList`; still dead wire surface).
3. **CLI `boss automation`** — #1042. Also had to build the runs/tasks/run RPC pairs #966 hadn't; `run` shipped as an engine stub.
4. **Second pool + dispatch routing** — #1043. Grew into a `drain_ready_queue` rewrite plus the worker-id-prefix scheme; both became foundations for the later review pool and spillover.
5. **Scheduler loop + occurrence math** — #1070. Hand-rolled cron parser; reformulated staleness rule; catch-up collapse; dispatched into a placeholder until #1077.
6. **Triage + outcome detection** — #1077. Marker parser + `finalize_automation_triage` (no Detector struct); `CreateAutomationTask` RPC; synthetic chore work items; made `boss automation run` real.
7. **App: Automations tab + schedule editor** — #1025 (merged before items 5/6; create wire-broken until #1098; run history followed later).
8. **App: pool switcher + backlog exclusion** — #1068 (exclusion later reversed by #1280).

## Open questions — how they resolved

1. **Synthetic Maintenance project (Q1):** stayed rejected. Provenance-by-FK shipped and carries routing, accounting, and links; the exclusion role it was also supposed to carry was reversed post-ship.
2. **Engine owns everything (Q2):** held completely.
3. **Triage prompt quality (Q3):** the worry was justified. The final-line marker protocol was kept (the alternative `boss automation triage-result` verb was never introduced), but it needed post-ship shoring up in exactly the predicted direction: an exactly-one-marker rule with an `Ambiguous` refusal state, DB verification of task markers, marker recovery for inconclusive runs, and a much longer preamble (single-shot mandate, already-tracked task list) — all responses to real field failures.
4. **Timezone/DST (Q4):** shipped as designed (fire-once-on-earliest for folds, run-slightly-later for gaps), with explicit bounds on the gap search.
5. **Open-task definition (Q5):** as proposed — fire-time enforcement; open = `todo|ready|doing|in_review|blocked` and not soft-deleted; `blocked` counts.
6. **`open_task_limit` default (Q6):** 1, as proposed; per-automation override in schema and CLI/app.
7. **Cron crate (Q7):** neither `croner` nor `cron` — a hand-rolled parser in the `boss-engine-automation-schedule` crate, chosen for purity, deterministic tests, and full control of DST policy.
8. **Suppressed-at-limit advancement (Q8):** advance-and-skip, as proposed, with a hold-if-no-following-occurrence refinement.
9. **Pool starvation within automations (Q9):** the risk materialized ("automation demand regularly exceeds the pool"). The answer was not per-automation fairness in `claim_worker` (still unbuilt) but pool growth (3 → 8) plus spillover into interactive slots with preemption.

## Known gaps (as of 2026-07-20)

Follow-up work this postmortem surfaced; each is tracked as a project task:

- **No semantic cron/timezone validation at any write path** — CLI, engine create/update, and the app editor all accept garbage that only fails per-tick in the scheduler; `validate_schedule()` exists and has zero callers.
- **Stale-abandoned occurrences are terminally mislabeled `failed_will_retry`** — the scheduler never emits `failed_gave_up` for a held occurrence whose catch-up window elapses; the code comment still defers this to "Maint task 6", which didn't deliver it.
- **The app never consumes `ListAutomationTasks` or `RunAutomation`** — no produced-task lifecycle list (the gap that forced the #1280 exclusion reversal) and no Run-now button, despite both RPCs and CLI verbs existing.
- **The edit sheet discards repo and enabled edits**; `AutomationPatch` can't clear optional overrides to NULL; `catch_up_window_secs` is settable nowhere.
- **Routing DB-error fallback** — a failed provenance lookup silently routes an automation task to the main pool.
- **Dead wire surface** — `FrontendEvent::AutomationRunResult` has never had a producer.
