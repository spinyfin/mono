# Boss: Engine event bus — event-driven reconcilers via an in-process message queue

- **Status:** design proposal (not yet implemented). This is a `kind=design` deliverable — architecture + migration plan, no code.
- **Project:** `proj_18c52e96fa5cbb18_e4` — "Engine event bus: event-driven reconcilers via an in-process message queue".
- **Provenance:** design execution `exec_18c52eaadb626758_f3`.
- **Related design docs:** [`work-subscriptions.md`](./work-subscriptions.md) (the one existing in-tree pub/sub — its topic-broker machinery is reused here); [`maintenance-tasks.md`](./maintenance-tasks.md) (catalogues the sweep/reconciler loops this doc reclassifies); [`auto-populate-project-tasks-on-design-pr-merge.md`](./auto-populate-project-tasks-on-design-pr-merge.md) (an existing event-on-merge trigger, and the consumer of this doc's task-breakdown section).
- **Related taxonomy projects:** P856 "PR/CI/conflict state reconciliation" (the reconciliation _domain_; this doc is the event-bus _architecture_ underneath it — coordinate scope, do not overlap), plus planned P707/P2264/P545.
- **Missing prior art:** `tools/boss/docs/investigations/github-event-detection-webhooks-vs-polling-2026-07-08.md` is cited ~8 times from `merge_poller.rs`/`app.rs` but is absent from the checkout. Its conclusions are recovered inline in [Non-goals](#non-goals) and [The external/GitHub class (b)](#class-b--external-state-pollers-out-of-scope-for-the-in-process-bus).

## TL;DR

The engine already does event-driven dispatch — but only through a handful of bespoke, single-purpose `tokio::sync::Notify` "kicks" and one in-transaction cascade. Everything else reconciles by polling on 30–300 s timers, which is why a design-postmortem task takes up to ~5 minutes to be _created_ after a project's last impl task drains. This doc proposes a single **typed, in-process topic event bus** that generalizes the proven kick pattern: producers `publish` state-transition events, reconcilers `subscribe` and react in sub-second time. The periodic sweeps stay, demoted to **backstops** — they catch dropped events, crash recovery, and inherently-external state. Because the sweeps remain the correctness floor, the bus can be **in-memory and best-effort**: a bus bug degrades to today's polling latency, never to lost work. This is exactly the primary-path-plus-backstop shape `dep_unblock_sweep` already ships; we are generalizing it, not inventing it.

## Goals

- **Cut user-visible reconcile latency for well-understood in-process transitions** from tens of seconds / minutes to sub-second. The headline offenders: design-postmortem creation (≤300 s today), orphan redispatch (≤60 s), stranded "Thinking…" clearing (≤60 s), leaked pool-claim release (≤60 s).
- **Generalize the existing bespoke `Notify`-kick primitives into one typed topic bus.** Today each edge-trigger (`coordinator.kick()`, `PrReconcilerTargetedKick`, `automation_scheduler_kick`, `shutdown_trigger`, `orphan_trigger`) is a hand-wired field with its own consumer wiring. New event-driven reconcilers should subscribe to a topic, not require a new plumbed-through `Arc<Notify>`.
- **Keep the sweeps as backstops, and make that relationship explicit.** The sweep is what makes an in-memory (non-durable) bus safe: any event the bus drops (subscriber panic, engine restart mid-flight, a producer that forgot to publish) is recovered within one sweep interval. No transition may depend on the bus for _correctness_ — only for _latency_.
- **Preserve the hard-won correctness properties already in the kick code** — most importantly the `scheduling_pending`/`scheduling_active` double-latch that closes the kick/drain TOCTOU (`coordinator.rs:1591`). A naive `Notify` bus that loses a wakeup arriving mid-drain would regress this.
- **Coexistence over cutover.** The bus ships alongside every sweep it accelerates. Each converted loop keeps its sweep at (possibly relaxed) cadence throughout. We measure the latency drop before we consider relaxing any sweep interval.

## Non-goals

- **GitHub / Buildkite webhook ingestion.** The external-state pollers (`merge_poller`, `trunk_queue_poller`, `external_tracker`, `syspolicyd_monitor`) poll because GitHub/Buildkite state changes without any in-process event to publish. The real fix there is inbound webhooks, analyzed in the (missing) `github-event-detection-webhooks-vs-polling-2026-07-08.md`. Recovered conclusions from its inline citations: **§8** keeps the periodic full sweep as the correctness backstop; **§9 item 3** replaces the single global tick with a per-PR adaptive timer (already partly shipped: `PrPollSchedule`, Hot 40 s / Cold 180 s in `merge_poller.rs:602-609`); **§9 items 3–4** add push-event + targeted single-PR reconcile (`PrReconcilerTargetedKick` is the plumbed-but-unfired entry point); **§9.2** adds conditional-request/ETag caching (`If-None-Match` → free `304`). Webhook ingestion is a **follow-up project**, out of scope here — though the internal bus this doc builds is the natural landing zone for a future webhook relay to publish onto.
- **A durable event log / replay system.** The bus is in-memory. Durability across restart is provided by the boot-time reconcile sweep, not by persisting events. (Matches `work-subscriptions.md`'s explicit non-goal of a durable event log in phase 1.)
- **Reworking the engine→app/UI event path.** `FrontendEvent` + `session_queue` topic delivery (outward, to clients) stays as-is. We _reuse its machinery_ for the internal bus but do not merge the two or change the wire protocol.
- **Removing any sweep.** No sweep is deleted by this project. A few may have their intervals relaxed _after_ latency is measured, but that is a follow-up decision gated on data, not part of v1.
- **A general job scheduler or cron replacement.** Timer-wheel-style scheduled events (envelope deadlines, automation cron) are in scope only as _bus event sources_; this is not "cron as a service."
- **Cross-process / cross-engine delivery.** One engine process, one bus. No distribution, no clustering.

## Background: what exists today (build on this, don't reinvent)

There is **no general in-process EventBus today.** The substrate is three proven-but-bespoke mechanisms plus one shared sweep scaffold.

### 1. The `Notify`-kick edge-trigger — the pattern to generalize

Each of these is a single-purpose edge-trigger — either a `tokio::sync::Notify` or a pair of atomic flags — wired as an explicit field, fired at specific call sites, and awaited/observed in one specific loop:

- **`ExecutionCoordinator::kick()`** (`coordinator.rs:2137`) — the engine's _primary_ dispatch path, already event-driven. Note this one is **not** a `Notify`: it's a pair of `AtomicBool`s (`scheduling_active` + `scheduling_pending`) plus a spawned `run_scheduler` task. `kick()` sets `scheduling_pending`, then `swap`s `scheduling_active`; if a scheduler is already alive it returns (the live task will observe pending), otherwise it spawns one. `run_scheduler` drains `list_ready_executions()`. That double-latch (`scheduling_pending` written _before_ contending on `scheduling_active`, `coordinator.rs:1586-1600` / `2137-2156`) is the reference correctness pattern: a kick arriving mid-drain is latched and re-enters the drain loop rather than being dropped. Fired from `release_worker_and_kick()`, the heartbeat re-kick (`coordinator.rs:2216`), `review.rs:664`, `engine_meta.rs:479/608`, and boot. Backstops: orphan_sweep (#18) + scheduler_heartbeat (15 s, #26).
- **`PrReconcilerTargetedKick` + `drain_pending()`** (defined in `merge_poller.rs:4211-4239`; held as a `ServerState` field at `app.rs:670`) — a thin struct: `notify: Arc<Notify>` + `pending: Arc<Mutex<Vec<String>>>` (a plain `Vec`, drained by `std::mem::take`). It carries a queue of specific PR URLs to reconcile one-at-a-time; consumed in the merge poller's `select!` wait arm (`merge_poller.rs:4608`). It is **plumbed but has no live producer yet** — an explicit entry point reserved for future push-event work. (It is a _targeted_ companion to the separate broad `pr_reconciler_kick: Arc<Notify>` at `app.rs:661`, which _is_ fired — via `notify_one()` on the `KickPrReconcilers` RPC — and triggers a full sweep.)
- **`automation_scheduler_kick: Arc<Notify>`** (`app.rs:676`) — notified by any automation mutation so the scheduler recomputes its min-next-fire sleep immediately rather than waiting out its cron interval.
- **`shutdown_trigger: Arc<Notify>`** (`app.rs:688`, awaited in the accept loop at `server.rs:1615`) and **`orphan_trigger: Arc<Notify>`** (local in `serve`, `server.rs:1565`, notified by the 1 s parent-pid watcher, awaited at `server.rs:1631`) — one-shot lifecycle signals.

**Takeaway:** the edge-trigger works and is battle-tested (including its TOCTOU fix). What's missing is _generality_: every new trigger needs a new field, new wiring, and a new consumer. A typed topic bus removes that per-trigger boilerplate.

### 2. The topic pub/sub machinery — reuse it

- **`ExecutionPublisher` trait (`coordinator.rs:~1420`) + `publish_*` on `ServerState` (`app.rs:1398-1454`)** already turn state changes into typed `FrontendEvent`s.
- **`session_queue.rs` `subscribe()`/`publish()`** (`app/session_queue.rs`) is a per-session outbound topic-fan-out broker with **per-topic coalescing** — at most one pending event per topic per session, newest-wins (`pending_topics: HashMap<String, usize>`, `session_queue.rs:106-174`). This coalescing model is directly relevant: for a _state-reconcile_ bus, "latest state wins per key" is exactly the right backpressure story (see [Backpressure](#backpressure--coalescing)).

This is designed and shipped for outward UI delivery (`work-subscriptions.md`). We lift the topic-broker + typed-event pattern for the internal bus; we do not reuse the _same instance_ (different audience, different lifecycle).

### 3. Existing event _sources_ (publishers, not yet on a bus)

- **`events_socket.rs`** — reverse unix socket carrying worker hook events into the engine. The real external event ingress; a natural bus publisher.
- **`DispatchEventSink` trait** (`engine/dispatch-events/src/lib.rs`) — dispatch-lifecycle telemetry sink. An event source, though not currently subscribe-able.
- **Per-slot `mpsc::UnboundedReceiver<Trigger>`** (`live_status_loop.rs:758`) — already hook-event-driven fan-in; the reference model for "event-first with a timer floor."

### 4. The primary-path + backstop template already in tree

**`dep_unblock_sweep`** (`dep_unblock_sweep.rs`) is the shape this whole project generalizes. Its own header says it plainly: the primary unblock path is the **in-transaction cascade** `cascade_dependents_after_prereq_status_change` (sub-second, fires inside the DB txn that writes a prereq to `done`/`archived`); the 30 s sweep is "an additional safety net… so any item the event path still misses (engine offline at transition time, or a future guard regression) is recovered within one sweep interval rather than hours." Every converted loop in this design adopts exactly this two-tier structure.

### 5. Shared scaffold

`sweep_loop.rs` (`spawn_sweep_loop` / `spawn_work_sweep_loop`): fire-on-boot, then `sleep(interval)` forever. Boot wiring lives in `app/server.rs` ~L889–L1553. The bus adds a sibling scaffold (`spawn_subscriber_loop`) without disturbing this one.

## Alternatives considered

### Alternative A — Keep adding bespoke `Notify` kicks per loop (status-quo evolution)

For each loop we want to accelerate, add another `Arc<Notify>` field to `ServerState`/coordinator, wire a producer call at the transition site, and add a `select!` arm to the consumer loop — exactly as `coordinator.kick` and `automation_scheduler_kick` are done today.

**Rejected.** It works (it's what ships now), but it does not scale to the ~8 loops we want to convert, let alone future ones. Each kick is O(N) wiring touching `app.rs`, the consumer module, and every producer site; there is no shared idempotency/latching/observability story, so every new kick re-derives (and risks re-breaking) the `scheduling_pending` TOCTOU fix by hand. It also gives no uniform place to add metrics ("events published/dropped per topic") or the boot-reconcile contract. The operator ask is explicitly for _"an event bus that things subscribe to"_ — a generalization — not more one-offs.

### Alternative B — A durable, persistent queue (SQLite outbox table)

Persist events to a `bus_events` table in the engine's SQLite DB (transactional-outbox pattern): producers insert an event row in the same txn as the state change; a dispatcher polls/tails the table and delivers to subscribers; delivered rows are marked done. This gives at-least-once delivery that survives restart without a boot sweep.

**Rejected for v1** (kept as a possible future upgrade). It's heavier than the problem needs: we _already_ have the sweeps as the durability mechanism, so persistence buys us nothing the boot-reconcile doesn't, at the cost of a new table, a migration, write amplification on every hot transition, and a compaction/GC loop. The outbox's own dispatcher still polls the table (trading a 60 s sweep for a tight DB poll). Crucially, it inverts the risk profile: a bug in a _durable_ queue can wedge or infinitely redeliver real work, whereas a bug in an _in-memory best-effort_ bus can at worst fall back to the sweep cadence we have today. We want the failure mode to degrade to "as slow as now," never "worse than now." If a future transition genuinely cannot tolerate the sweep backstop (none identified today), an outbox for _that topic only_ is a clean follow-up.

### Alternative C — An external message broker (Redis / NATS / embedded broker)

Run or embed a real broker and publish/subscribe across it.

**Rejected.** Massively out of proportion. The engine is a single process on someone's laptop; the bus is in-process fan-out between tokio tasks that already share memory and an `Arc<ServerState>`. An external broker adds an operational dependency, a network hop, serialization, and a new failure domain to eliminate — for zero benefit over an in-memory `tokio` channel. Cross-process distribution is an explicit non-goal.

### Alternative D (chosen) — In-memory typed topic bus + sweeps as backstop

A single in-process `EventBus` owning a set of typed topics. Producers `publish(Event)`; the bus fans out to per-subscriber bounded channels; each reconciler runs a small subscriber loop that reacts to events and _also_ keeps its existing sweep as a backstop. In-memory, best-effort, coalescing under pressure. Durability = boot-time reconcile sweep + steady-state backstop sweeps.

**Chosen** because it is the minimal generalization of the proven in-tree pattern (`dep_unblock_sweep`'s primary-cascade-plus-backstop, and the `Notify`-kick edge-trigger), it makes the sweep-as-safety-net relationship a first-class invariant rather than an accident, and its worst-case failure mode is "today's latency." See below.

## Chosen approach

### Overview

Introduce an `EventBus` in `tools/boss/engine/core` (candidate: its own small crate `boss-event-bus` under `tools/boss/engine/`, per the repo's prefer-crates convention — it has its own vocabulary, tests, and multiple consumers, and a one-way `engine-core → event-bus` edge with no back-reference). The bus holds:

- a set of **typed topics**, each keyed by an enum variant + optional entity key (`ProjectId`, `TaskId`, `ExecutionId`, `HostId`, `PrUrl`);
- for each subscriber, a **bounded `tokio::sync::mpsc` channel** (or a coalescing mailbox — see backpressure);
- a `publish(event)` that fans the event out to every matching subscriber, non-blocking;
- a `subscribe(topic_filter) -> Subscription` that a reconciler's loop awaits.

Each reconciler becomes: `select! { event = subscription.recv() => reconcile_targeted(event), _ = sweep_timer.tick() => reconcile_full() }`. The sweep arm is the backstop; the event arm is the latency win. This mirrors `live_status_loop`'s "event-first with a 60 s floor."

### Event taxonomy (initial topics)

Events are coarse state-transition facts, **not** commands. Each carries the minimal key needed to reconcile; the subscriber re-reads authoritative state from the DB (events are hints, never the source of truth — this is what keeps them idempotent and loss-tolerant).

| Event                                                              | Published when / from                                     | Primary subscriber(s)                                                               |
| ------------------------------------------------------------------ | --------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| `TaskTerminal { task_id, project_id }`                             | a task reaches `done`/`archived` (the status-write txn)   | project_postmortem, dep-unblock cascade                                             |
| `ProjectImplDrained { project_id }`                                | last non-terminal impl task of a project reaches terminal | project_postmortem (schedule design_postmortem)                                     |
| `ExecutionTerminal { execution_id, task_id, host_id, pool_claim }` | an execution reaches a terminal state                     | pool_claim release, orphan redispatch, terminal_work reap, stranded-answering clear |
| `PrMerged { pr_url, task_id }`                                     | merge poller confirms a merge                             | auto-populate trigger, chore-lifecycle, dep-unblock                                 |
| `HostDisabled { host_id }`                                         | a host is marked offline/disabled (in-process)            | host_reconcile (terminalize/re-route its executions)                                |
| `DependencyPrereqsSatisfied { task_id }`                           | a prereq transition clears the last block                 | dep-unblock (already the cascade; formalize as a bus event)                         |
| `TransientErrorIdle { execution_id }`                              | worker reports a transient API error                      | transient_recovery (auto-resume)                                                    |
| `AnswerAgentDied { execution_id }`                                 | answer-agent pane dies with a pending question            | stranded_answering (clear "Thinking…")                                              |
| `PrReconcileRequested { pr_url }`                                  | review/merge lifecycle wants one PR re-checked            | merge poller targeted reconcile (generalizes `PrReconcilerTargetedKick`)            |
| `DispatchReady`                                                    | a `ready` execution is enqueued                           | coordinator dispatch (generalizes `coordinator.kick()`)                             |
| `Timer { deadline_id }`                                            | a timer-wheel deadline elapses                            | envelope_watch, automation cron                                                     |

This list is the _initial_ set; the bus API is open to new variants without touching unrelated code (a new topic is a new enum arm + a producer + a subscriber, nothing else).

### Publish / subscribe API (sketch)

```rust
// boss-event-bus
pub enum Event { /* variants above, each carrying its keys */ }

pub struct EventBus { /* topic → Vec<subscriber sender> */ }

impl EventBus {
    /// Non-blocking, infallible from the caller's view. Fans out to every
    /// matching subscriber's mailbox; if a mailbox is full, coalesces
    /// (see backpressure). Never awaits, never blocks the producer's txn.
    pub fn publish(&self, event: Event);

    /// Returns a Subscription the caller awaits in its reconcile loop.
    /// `filter` selects topic variants (and optionally an entity key).
    pub fn subscribe(&self, filter: TopicFilter) -> Subscription;
}

pub struct Subscription { /* rx */ }
impl Subscription {
    pub async fn recv(&mut self) -> Option<Event>;
}
```

`publish` is callable from inside a DB transaction closure only in the sense that it is cheap and synchronous — but the **canonical pattern is publish-after-commit**: enqueue the event in memory and fire it once the txn has committed, so a rolled-back transition never emits a false event. (For the dep-unblock cascade, which already runs inside the txn, we keep the in-txn cascade as the primary path and additionally emit the bus event post-commit for other subscribers.)

### Delivery semantics

- **At-most-once, best-effort, from the bus's perspective.** The bus does not retry or persist. A dropped event (full mailbox that coalesced it away, subscriber restart, engine crash) is _by design_ recovered by the backstop sweep. This is only safe because every subscriber's sweep is retained.
- **Effective at-least-once for the _system_,** delivered by the union of (fast bus event) ∪ (slow backstop sweep). The sweep guarantees eventual reconciliation; the bus guarantees it's usually immediate.
- **Idempotency is mandatory and already satisfied.** Every reconciler is _already_ idempotent because the sweeps re-run it on a timer — reconciling the same transition twice must be a no-op. This is the invariant that makes best-effort delivery acceptable. Part of each conversion task is an explicit test asserting "reconcile called twice for the same key = one effect." (Confirmed by inspection for dep-unblock, pool-claim, orphan; each conversion task re-confirms for its loop.)

### Ordering and per-key serialization

- **No global ordering.** Events across different keys may be delivered in any order.
- **Per-key serialization where a reconciler needs it** is provided by the subscriber loop processing its mailbox sequentially, plus the reconciler taking the same per-entity lock the sweep already takes (e.g. `spawn_pane_lock`, per-execution state). We do **not** add a new ordering primitive; the reconcilers already tolerate arbitrary ordering because sweeps deliver in arbitrary order today.
- The one subtle case is the coordinator drain: its `scheduling_pending`/`scheduling_active` double-latch must be preserved. The bus subscriber for `DispatchReady` wraps _exactly_ that latch — a `DispatchReady` arriving while the drain is mid-flight sets pending and re-enters, identical to today's `kick()`. This is called out as a dedicated migration task with the existing TOCTOU test carried over.

### Durability across restart

There is none in the bus, and that's the design. On engine boot, every sweep already fires immediately (the `sweep_loop` fire-on-boot behavior). That boot pass **is** the recovery mechanism: any transition that happened while the engine was down, or any event in flight when it crashed, is reconciled by the first sweep tick. The design makes this explicit: **every converted loop keeps fire-on-boot**, and the boot sweep is documented as the crash-recovery contract. No event survives a restart, and none needs to.

### Backpressure & coalescing

- Each subscriber mailbox is **bounded** (small, e.g. 256). A producer never blocks.
- On a full mailbox, the bus **coalesces per (topic, key)** — newest event wins, mirroring `session_queue`'s `pending_topics` newest-wins coalescing. This is correct for reconcile events precisely because they are _state hints, not commands_: if `ExecutionTerminal{exec_42}` is already pending and another arrives, collapsing them loses nothing — the subscriber re-reads current state anyway.
- If coalescing still can't keep up (pathological burst), the overflow is a **dropped event → backstop sweep catches it**, and the bus increments a `bus_events_dropped_total{topic}` counter so the condition is observable rather than silent (per the repo's no-silent-caps norm and the existing `metrics::Registry`).

### Subscriber failure / retry without losing the backstop

- A subscriber loop that **panics** is restarted by a supervising `JoinHandle` watcher (same shape as existing spawned loops), and on restart it does a full reconcile pass (identical to boot), so nothing is stranded.
- A subscriber that **errors on one event** (e.g. DB hiccup) logs, increments a counter, and moves on — the backstop sweep will retry the same reconcile within one interval. We deliberately do **not** build per-event retry/backoff into the bus; retry is the sweep's job. This keeps the bus dumb and the failure mode bounded.

### Generalizing the existing Notify-kicks onto the bus

Three concrete migrations prove the generalization (each is a task in the breakdown):

1. **`coordinator.kick()` → `DispatchReady` topic.** The coordinator subscribes to `DispatchReady`; producers that today call `.kick()` instead `publish(DispatchReady)`. The subscriber wrapper preserves the `scheduling_pending`/`scheduling_active` latch verbatim. Backstops (orphan_sweep, scheduler_heartbeat) unchanged. This is the highest-risk migration (it's the primary dispatch path) and ships behind a flag with the existing TOCTOU test as the gate.
2. **`PrReconcilerTargetedKick` → `PrReconcileRequested` topic.** The `Arc<Mutex<Vec<String>>>` queue-of-PR-urls becomes a keyed topic; the merge poller subscribes for targeted single-PR reconciles. Because the targeted kick has _no live producer today_, this migration also lands its first real producers: the PR-lifecycle sites (review-completion in `review.rs`, merge/engine-meta transitions in `engine_meta.rs`) publish `PrReconcileRequested{pr_url}` at the moment they change a PR's state, instead of relying on the next full merge-poller sweep.
3. **`automation_scheduler_kick` → an automation-mutation topic.** Mutation handlers publish; the scheduler subscribes to recompute its min-next-fire.

`shutdown_trigger`/`orphan_trigger` are intentionally **left as bespoke `Notify`** — they are one-shot lifecycle signals with a single producer and consumer, where the bus adds nothing. Generalizing them is explicitly _not_ a goal (avoid churn for its own sake).

### Per-loop disposition

Class **(a)** = clean in-process event-bus candidate (convert). Class **(b)** = poll-bound on external GitHub/Buildkite state (webhook-later; out of scope). Class **(c)** = keep as backstop-only (crash/restart/liveness scan with no in-process event to publish).

#### Class (a) — convert to event-driven (keep sweep as backstop)

| Loop (file:line)                               | Interval today       | Transition it subscribes to                           | Publisher site                                  |
| ---------------------------------------------- | -------------------- | ----------------------------------------------------- | ----------------------------------------------- |
| `project_postmortem_sweep` (`server.rs:1380`)  | 300 s                | `ProjectImplDrained`                                  | last-impl-task terminal status write            |
| `orphan_sweep` (`server.rs:1276`)              | 60 s                 | `ExecutionTerminal` (no live execution → redispatch)  | execution death; partly via coordinator already |
| `stranded_answering_sweep` (`server.rs:1125`)  | 60 s                 | `AnswerAgentDied`                                     | answer-agent pane death                         |
| `pool_claim_sweep` (`server.rs:1021`)          | 60 s                 | `ExecutionTerminal` (release leaked claim)            | execution terminal                              |
| `terminal_work_sweep` (`server.rs:1042`)       | 60 s ×2-pass         | `ExecutionTerminal` (reap live pane on terminal item) | execution/work terminal                         |
| `envelope_watch` (`server.rs:1179`)            | 60 s                 | `Timer{deadline}` (per-execution duration deadline)   | timer-wheel                                     |
| `transient_recovery` (`server.rs:1241`)        | 60 s                 | `TransientErrorIdle` (auto-resume)                    | worker transient-error hook                     |
| `host_reconcile` (`server.rs:1325`)            | 60 s                 | `HostDisabled`                                        | host disable (in-process)                       |
| `automation_scheduler` (`server.rs:1418`)      | adaptive cron        | automation-mutation event + `Timer` (cron)            | mutation handlers; timer-wheel                  |
| `coordinator dispatch` (`coordinator.rs:2137`) | kick + 15 s backstop | `DispatchReady`                                       | `ready` execution enqueue                       |

Reference model (already this shape, confirm and keep): **`live_status_loop`** (event-driven via hooks, 60 s is only a floor) and **`dep_unblock_sweep`** (`server.rs:1363`, 30 s; primary is the in-txn cascade, sweep is the stated backstop — the template).

#### Class (b) — external-state pollers (out of scope for the in-process bus)

These poll because the authoritative state lives in GitHub/Buildkite and changes with no in-process event. The correct acceleration is **inbound webhooks**, a separate project. Keep polling as the backstop; note the webhook direction.

| Loop                                            | Cadence                                           | Note                                                                                                                                                                                                                                                                       |
| ----------------------------------------------- | ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `merge_poller` (`server.rs:938`)                | 60 s full + adaptive Hot 40 s / Cold 180 s per-PR | Already partly kick-driven (`PrReconcilerTargetedKick`). Drives `ci_watch` + `conflict_watch` + `ci_rebounce` (event-handler modules, _not_ independent loops — all GitHub-derived latency collapses to this cadence). Webhooks are the real fix (missing doc §8/§9/§9.2). |
| `trunk_queue_poller`                            | 15 s / 10 min                                     | External trunk-merge-queue state.                                                                                                                                                                                                                                          |
| `external_tracker reconcile` (`server.rs:1338`) | 120 s                                             | GitHub Projects sync.                                                                                                                                                                                                                                                      |
| `syspolicyd_monitor` (`server.rs:1230`)         | 10 s                                              | OS-level state.                                                                                                                                                                                                                                                            |

The internal bus is nonetheless the right _landing zone_ for a future webhook relay: a relay would `publish(PrReconcileRequested{pr_url})` and the merge poller's existing targeted path handles it — no new plumbing.

#### Class (c) — keep periodic (backstop-only, by design)

These reconcile state left by **crashes, filesystem/pid/ssh death, or external liveness** — there is no in-process event to publish because the thing that changed is _the absence of a process/host/file_, observed only by scanning. Making them event-driven is impossible in principle (you cannot get an event from a crashed process), so they stay periodic and that scope boundary is _principled, not arbitrary_:

`dead_pid_sweep`, `cube_lease_heartbeat` (300 s), `ladder_lease_heartbeat`, `husk_pane_sweep`, `lost_workspace_sweep`, `dead_pane_sweep`, `remote_lease_reconcile`, `stale_worker_sweep` (30 min), `spawn_ack_sweep`, `execution_retention_sweep` (GC, 3600 s), `dispatch_failure_recovery_sweep`, `pr_review_recovery`, dispatch `stage_stalled_detector` (15 s) + `dispatch_stall_escalation`, stalled-spawn detector (10 s), `scheduler_heartbeat` (15 s), `database_backup` (3600 s), metrics flush, parent-pid watcher (1 s). `transient_recovery`'s _backstop half_ also stays (its event half is class (a)).

### Phased, low-risk migration

The invariant across all phases: **the bus never replaces a sweep; it races it.** A bus bug degrades to the sweep's latency, never to lost work.

- **Phase 0 — Bus core + one pilot.** Land `boss-event-bus` (types, `publish`/`subscribe`, bounded coalescing mailbox, drop-counter metric, subscriber-restart supervision) with unit tests. Convert exactly one loop end-to-end as the proof: **`project_postmortem_sweep`** (highest user-visible payoff, self-contained, `ProjectImplDrained` is a clean transition). Keep its 300 s sweep untouched. Measure: time-from-drain-to-postmortem-created, before vs after.
- **Phase 1 — Generalize the kicks.** Migrate `coordinator.kick` (behind a flag, TOCTOU test as gate), `PrReconcilerTargetedKick`, `automation_scheduler_kick` onto the bus. This retires bespoke wiring and validates the double-latch preservation on the riskiest path.
- **Phase 2 — Convert the remaining class-(a) execution-terminal loops.** `pool_claim_sweep`, `orphan_sweep`, `terminal_work_sweep`, `stranded_answering_sweep`, `transient_recovery` (event half), `host_reconcile`. These mostly share the `ExecutionTerminal`/`HostDisabled` publishers, so they land as a cluster of small subscriber conversions once the publishers exist.
- **Phase 3 — Timer-wheel events.** `envelope_watch` and automation cron move onto `Timer` events from a shared timer-wheel source.
- **Phase 4 — Measure, then (maybe) relax intervals.** With latency data in hand, propose relaxing a few backstop intervals (e.g. 60 s → 300 s) _as a separate, operator-approved decision_. Not a v1 blocker; explicitly deferred.

Each phase is independently shippable and independently reversible (drop the subscriber, the sweep still runs).

## Risks / open questions

- **Publish-after-commit vs in-txn publish.** Publishing after the DB commit avoids false events on rollback but opens a tiny window where the engine crashes post-commit, pre-publish — recovered by the boot sweep, so acceptable. Confirm this is the rule everywhere (the dep-unblock cascade is the one place that legitimately acts _inside_ the txn; it keeps doing so and _additionally_ emits post-commit for other subscribers). **Want a reviewer to bless "publish-after-commit is the default, in-txn cascade is the documented exception."**
- **Coordinator double-latch preservation is load-bearing.** If the `DispatchReady` migration subtly breaks the `scheduling_pending`/`scheduling_active` latch, we regress the exact TOCTOU that stranded ready executions before (`coordinator.rs:1591` incident). Mitigation: flag-gated rollout + carry the existing regression test. Still the single scariest change — flagged for extra review scrutiny.
- **Do we need per-key ordering guarantees anywhere?** Current reconcilers tolerate arbitrary order (sweeps deliver arbitrarily). If any converted reconciler turns out to _require_ ordered delivery, that's a redesign for that topic. Believed none do; each conversion task must confirm.
- **Mailbox size / coalescing tuning.** 256 + coalesce is a guess. The drop-counter metric makes it tunable, but the initial number is unvalidated under real burst load.
- **Scope boundary with P856 (PR/CI/conflict reconciliation).** P856 owns the _reconciliation domain_; this doc owns the _bus architecture_. The `PrReconcileRequested` topic and merge-poller targeting sit on that seam. **Want a reviewer/coordinator to confirm the split** so we don't double-implement the targeted-PR path.
- **Missing webhooks investigation doc.** `github-event-detection-webhooks-vs-polling-2026-07-08.md` is referenced but absent. This doc folds in its recoverable conclusions but the full analysis is lost. Should someone recover/rewrite it before the webhook follow-up project starts? (Out of scope here, but flagged.)

## Proposed implementation task breakdown

Tasks are PR-sized (one subsystem, one worker, one session) and listed in dependency order. "Depends on" names gating entries. Tasks at the same depth with no shared files may run in parallel (noted).

### 1. Event bus core crate

**Scope:** Create the `boss-event-bus` crate under `tools/boss/engine/`: the `Event` enum (initial variants from the taxonomy table), `EventBus` with `publish` (non-blocking fan-out) and `subscribe(filter) -> Subscription`, bounded per-subscriber mailboxes with per-(topic,key) newest-wins coalescing, a `bus_events_dropped_total{topic}` counter wired to the existing `metrics::Registry`, and a subscriber-loop supervision helper that restarts a panicked subscriber with a full-reconcile-on-restart contract. Unit tests for fan-out, coalescing-under-pressure, and drop-counting. No producers or subscribers wired yet — pure infrastructure with minimal, one-way `engine-core → event-bus` dependency edge.

**Effort:** `medium`

**Depends on:** none.

### 2. Publish-after-commit helper + producer convention

**Scope:** Add the small engine-core seam that lets a state-write path enqueue an event to publish _after_ its DB transaction commits (and drop it on rollback), plus docs/tests establishing "publish-after-commit is the default." No transition emits yet — this is the shared plumbing the producer tasks below use. Single-subsystem (engine-core).

**Effort:** `small`

**Depends on:** Event bus core crate.

### 3. Pilot conversion — project_postmortem via `ProjectImplDrained`

**Scope:** Emit `ProjectImplDrained{project_id}` when a project's last non-terminal impl task reaches terminal (publish-after-commit); subscribe `project_postmortem` to it and schedule the design_postmortem immediately on the event. Keep the 300 s sweep as the untouched backstop. Add an idempotency test (event + sweep both fire = one postmortem). Add before/after latency logging. This is the end-to-end proof of the architecture.

**Effort:** `medium`

**Depends on:** Event bus core crate; Publish-after-commit helper.

### 4. Coordinator dispatch on the bus (`DispatchReady`), flag-gated

**Scope:** Introduce the `DispatchReady` topic; publish it wherever a `ready` execution is enqueued; subscribe the coordinator, wrapping the existing `scheduling_pending`/`scheduling_active` double-latch verbatim so a mid-drain event re-enters the drain. Flag-gated (default off initially). Carry over the existing kick/drain TOCTOU regression test as the gate. Leave `coordinator.kick()` in place behind the flag for instant rollback. Highest-risk task — single-subsystem but demands extra review.

**Effort:** `medium`

**Depends on:** Event bus core crate; Publish-after-commit helper.

### 5. `PrReconcilerTargetedKick` → `PrReconcileRequested` topic

**Scope:** Replace the bespoke targeted-kick PR-url queue with a keyed `PrReconcileRequested{pr_url}` topic; have `review.rs` and `engine_meta.rs` publish; subscribe the merge poller's targeted single-PR reconcile path to it. Coordinate with P856 on the seam. Keep the broad `pr_reconciler_kick` sweep behavior as backstop.

**Effort:** `medium`

**Depends on:** Event bus core crate; Publish-after-commit helper.

### 6. `automation_scheduler_kick` → automation-mutation topic

**Scope:** Publish an automation-mutation event from the create/update/enable/disable/delete handlers; subscribe the automation scheduler to recompute its min-next-fire sleep. Retire the `automation_scheduler_kick: Arc<Notify>` field. Single-subsystem.

**Effort:** `small`

**Depends on:** Event bus core crate; Publish-after-commit helper.

_Tasks 4, 5, 6 are the "generalize the kicks" cluster and may run in parallel with each other and with task 3 — they touch disjoint subsystems (coordinator, merge poller, automation scheduler). Each independently depends only on tasks 1 and 2._

### 7. `ExecutionTerminal` / `HostDisabled` publishers

**Scope:** Emit `ExecutionTerminal{execution_id, task_id, host_id, pool_claim}` at every execution-terminal transition and `HostDisabled{host_id}` on host disable (publish-after-commit). No subscribers yet — this is the shared publisher the class-(a) execution-terminal cluster (tasks 8–11) consumes, split out so those land as thin subscriber PRs. Single-subsystem (engine-core state writes).

**Effort:** `medium`

**Depends on:** Event bus core crate; Publish-after-commit helper.

### 8. Convert pool_claim_sweep + terminal_work_sweep to subscribe `ExecutionTerminal`

**Scope:** Subscribe both reap paths (leaked pool-claim release; live-pane-on-terminal-item reap) to `ExecutionTerminal`, reconciling the specific execution immediately. Keep both 60 s sweeps as backstops. Idempotency tests. These two co-edit the terminal-reap area, so land them as one PR.

**Effort:** `small`

**Depends on:** `ExecutionTerminal` / `HostDisabled` publishers.

### 9. Convert orphan_sweep to subscribe `ExecutionTerminal`

**Scope:** On `ExecutionTerminal` with no live execution for an `active` item, redispatch immediately (it's partly coordinator-kick-driven already; formalize onto the bus). Keep the 60 s sweep backstop. Idempotency test.

**Effort:** `small`

**Depends on:** `ExecutionTerminal` / `HostDisabled` publishers.

### 10. Convert stranded_answering_sweep + transient_recovery (event half)

**Scope:** Subscribe stranded-answering clearing to `AnswerAgentDied` and transient auto-resume to `TransientErrorIdle` (publish these from the worker-hook ingress in `events_socket.rs`). Clear "Thinking…" / auto-resume on the event. Keep both 60 s sweeps (transient keeps its full backstop half). Idempotency tests. Two disjoint reconcilers but both wire new publishers into the same hook-ingress path — land together to avoid double-editing `events_socket.rs`.

**Effort:** `medium`

**Depends on:** Event bus core crate; Publish-after-commit helper.

### 11. Convert host_reconcile to subscribe `HostDisabled`

**Scope:** On `HostDisabled`, terminalize/re-route that host's executions immediately. Keep the 60 s sweep backstop. Idempotency test.

**Effort:** `small`

**Depends on:** `ExecutionTerminal` / `HostDisabled` publishers.

_Tasks 8, 9, 11 all subscribe to the publishers from task 7 and touch disjoint reconcile modules — they may run in parallel. Task 10 depends only on tasks 1–2 (different publisher source) and may run in parallel with the whole task-7 cluster._

### 12. Timer-wheel event source

**Scope:** Introduce a shared timer-wheel that emits `Timer{deadline_id}` events onto the bus. No consumers yet — infrastructure for tasks 13. Single-subsystem.

**Effort:** `medium`

**Depends on:** Event bus core crate.

### 13. Convert envelope_watch + automation cron to `Timer` events

**Scope:** Subscribe `envelope_watch` (per-execution duration deadlines) and the automation cron fire to `Timer` events from the timer-wheel. Keep envelope_watch's 60 s sweep backstop. Idempotency tests. These are separate reconcilers over the same new source — may split into two PRs (envelope; automation) if the wheel API is stable after 12.

**Effort:** `medium`

**Depends on:** Timer-wheel event source; (automation portion also) `automation_scheduler_kick` → automation-mutation topic.

### 14. Latency measurement + backstop-interval relaxation proposal

**Scope:** Add/collect the before/after latency metrics across the converted loops (drain→postmortem, execution-terminal→reap, etc.), summarize the observed drop, and produce an **operator-facing proposal** for which backstop intervals could be relaxed (e.g. 60 s → 300 s). This is a validation/analysis task, _not_ an autonomous interval change — the actual relaxation is a human decision. Depends on the converted loops existing and running long enough to gather data.

**Effort:** `small`

**Depends on:** Pilot conversion (task 3) and the class-(a) conversion cluster (tasks 8–11, 13).

### Deferred / out of scope (`future / not a v1 blocker`)

- **GitHub/Buildkite webhook ingestion** (class (b) acceleration): a separate project. Would `publish(PrReconcileRequested)` onto this bus from an inbound webhook relay; no new bus plumbing needed. _Deferred._
- **Durable outbox for any topic that cannot tolerate the sweep backstop:** none identified today; add per-topic only if a future transition needs it. _Deferred._
- **Retiring `shutdown_trigger` / `orphan_trigger` onto the bus:** intentionally left bespoke (single producer/consumer lifecycle signals); no value in converting. _Explicitly not planned._
- **Relaxing any class-(c) sweep:** they reconcile crash/liveness state with no publishable event; they stay periodic by design. _Not planned._
- **Recovering/rewriting the missing `github-event-detection-webhooks-vs-polling-2026-07-08.md`:** prerequisite for the webhook project, not for this one. _Deferred to that project._
