# Boothby — Boss's autonomous groundskeeper ("the Final Boss")

Boothby (after the Starfleet Academy groundskeeper) is an autonomous, coordinator-privileged maintenance agent. His primary job is keeping the Boss taxonomy tidy — closing stale and duplicate tasks, merging attention items, archiving empty projects, unwedging stuck work. His secondary job is improving Boss itself — mining the engine's logs and agent transcripts for errors, anomalies, and friction, and filing chores or `boss shake` issues. He is "the Final Boss": full coordinator-level access plus elevated powers the ordinary coordinator lacks (notably: reading the coordinator's own transcripts). Every action he takes is durably journaled, visible in a dedicated UI pane, and — where the action is reversible — undoable with one click.

## Goals

- A **periodically-woken maintenance agent** with both timer-based and event-based activation, plus manual fire.
- **Grounds-keeping actions** with coordinator-level access via the `boss` / `bossctl` surface: close no-longer-relevant tasks, close duplicates in favor of a canonical, close/merge stale or duplicate attention items, archive empty projects, and unwedge stuck work (dead-worker/dead-lease, yellow idle-park, orphaned executions, SlotBusy fallout, Doing-with-no-live-run — full catalogue below).
- **Additional maintenance actions** where sensible: reaping dead-but-live executions, expiring stale leases, pruning abandoned revisions, reconciling PR state vs task state, nudging stalled reviews, garbage-collecting orphaned workspaces via cube, recovery-patch GC, and re-running the effort heuristic on drifted rows.
- **Self-improvement pipeline**: watch engine logs for errors and anomalies, mine worker _and coordinator_ transcripts for friction patterns, and file findings — as **chores against the Boss product** on a Boss developer machine, or as **GitHub issues via `boss shake`** otherwise — without ever duplicating his own prior filings.
- **Hard requirement — full auditability with undo**: a durable, engine-owned audit trail of every action (what/why/when), a UI pane that renders it, a per-action-type reversibility classification, and a working undo mechanism for the reversible types.
- **Safety rails**: per-pass blast-radius caps, a propose (dry-run) mode, guards that prevent fighting live workers or the human coordinator, and scoping of all destructive verbs to the local install root.
- **Idempotence & convergence**: repeated wakeups never flap (close→reopen→close) and never re-file the same finding; a human reversal of a Boothby action permanently suppresses that action until the operator says otherwise.
- **Observability of Boothby himself**: durable pass history, his session transcripts, and his own log stream, all reachable from the UI.

## Non-goals

- **Replacing the deterministic sweep fleet.** The engine's ~20 reconcilers (`orphan_sweep`, `dead_pid_sweep`, `stale_worker_sweep`, `merge_poller`, `cube_lease_heartbeat`, … — all wired in `engine/core/src/app/server.rs::serve` on the `sweep_loop::spawn_sweep_loop` scaffold) remain the fast, deterministic first line of defense. Boothby is the _judgment_ layer above them: he handles what the sweeps flag but cannot decide (semantic staleness, duplication, `Unknown` liveness verdicts, parked rows) and the gaps they deliberately leave. No existing sweep is removed or weakened.
- **Doing feature work.** Boothby never edits repos, never opens PRs, never implements chores he files. His writes are taxonomy mutations, operational unwedging, and issue/chore filing — nothing else.
- **Auto-merging or auto-approving anything on GitHub.** He may nudge a stalled review (attention item), never act on the PR itself.
- **Cross-instance or cross-machine action.** Boothby acts only on the engine instance that spawned him (its `state.db`, its cube install, its workspaces). Remote hosts registered with the engine are out of scope for v1 (`future`).
- **A general rules engine / user-programmable policies.** v1 ships a fixed action catalogue with per-verb autonomy settings; a user-defined policy language is explicitly out of scope.
- **Replacing the Automations feature** (`maintenance-tasks.md`). Automations run _repo_ maintenance (clippy, dep bumps) producing PRs via product-scoped triage agents; Boothby runs _taxonomy and engine_ maintenance. They coexist; Boothby may reuse its scheduler patterns but shares no rows with it.
- **Undo for inherently irreversible operations.** Killing a process, force-releasing a lease, or deleting a recovery patch cannot be un-done; for these the requirement is audit + conservative gating, not undo (classification below).

## Background: what exists today (grounding)

Facts the design builds on, so reviewers don't have to re-derive them:

- **Sweep fleet**: all periodic reconcilers live in `engine/core/src/app/server.rs::serve` on `spawn_sweep_loop` (fire once at boot, then per-interval). Known residual gaps flagged in-tree: recovery patches under `<state_root>/recovery/*.patch` are never GC'd (`docs/post-crash-recovery.md`); closed-unmerged PRs collapse the task to `done` because `chore-lifecycle-pr-closed-unmerged.md` is unbuilt (`work/pr_flow.rs`); boot-time `run_reconcile.rs` `Unknown` liveness verdicts are only `warn!`-logged; `NudgeBreakerParked` / `waiting_human` rows have no auto-unpark; abandoned revision rows are never pruned; `envelope_watch.rs` and `syspolicyd_monitor.rs` are detect-only.
- **Automations** (built): `automation_scheduler.rs` (adaptive sleep to next `next_due_at`, `kick` Notify), `automation_triage.rs` (`TriageDispatcher`, decision-marker parsing), `automation_runs` history with an outcome string enum, `tasks.source_automation_id` provenance, a dedicated pool, an Automations tab. This is the closest architectural precedent for "scheduled engine-spawned agent with an audited run history."
- **Audit primitives**: `project_property_audit` (`old_value`/`new_value`/`actor`/`changed_at` — the pre-image shape), `planner_runs` + `boss project unpopulate` (the batch-undo precedent, with `PlannerUndoButton` in the app), `editorial_actions`, dispatch-event JSONL streams (`dispatch_events.rs`, `<state_root>/dispatch-events/current.jsonl`), and `reconcile_audit.rs::append_reconcile_audit` (`[engine-reconcile]` description lines).
- **Provenance / actor primitives**: `tasks.created_via` (accepts prefixed opaque values via `is_known_created_via`, e.g. `ci-fix:<id>`), `last_status_actor` (`human|boss|engine`), `AttentionMerge` ledger rows carrying `model` + `decision_rationale`.
- **Undo-shaped verbs that already exist**: `boss task delete` is a soft-delete (`deleted_at` tombstone) with `boss task restore` as its exact inverse; `boss project delete` archives (never hard-deletes); `archived` task status is reversible by another status move.
- **Privilege machinery**: `RpcTier { User, AppOrBoss, BossOnly }` with two trust roots (`app_pid`, `boss_pid` via `RegisterBossSession`) enforced by `authorize_rpc` peer-pid ancestry (`engine/core/src/app.rs`). Coordinator-ness is "bossctl on PATH + pid ancestry", not a credential.
- **Agent spawn path**: `runner.rs` → `spawn_flow.rs::start_worker` → `worker_setup.rs` (renders `.claude/CLAUDE.md` + hook-wired `settings.json`) → `EngineToAppRequest::SpawnWorkerPane` → `WorkerRegistry`. A capability-restricted template exists: the answer agent (`answer_agent.rs`, `WorkerKind::Answer`, forced permission mode, deny-by-default allowlist).
- **Logs & transcripts**: engine structured log at `<state_root>/engine-trace.jsonl` (size-rotated, read via the `boss-log-files` crate), lifecycle events in `engine-audit.log`, IPC JSONL day-rotated under `<state_root>/ipc/`. Worker transcripts are Claude JSONL at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`, with `session_id`/`transcript_path` persisted on `work_runs`; `boss-transcript-markdown` parses them. **Nothing today records or reads the coordinator's own transcript** — it lands in the same `~/.claude/projects/` tree but the engine holds no pointer to it.
- **Filing surfaces**: `boss shake` files a GitHub issue against `spinyfin/mono` via an embedded GitHub App (supports `--label`, `--dry-run`; gathers no context itself). The settings registry (`engine/core/src/settings.rs`) already has `coordinator.direct_developer_mode` (default false): "files Boss bugs/features as chores against the Boss product instead of GitHub issues."

## Alternatives considered

### A1 — Extend the deterministic sweep fleet (no agent, no LLM)

Encode each maintenance rule as another `spawn_sweep_loop` reconciler: a staleness sweep, a duplicate-title sweep, an empty-project sweep, a log-grep sweep that files chores on ERROR bursts.

Rejected. The high-value half of Boothby's job is _semantic_: "is this task still relevant given the project's design doc and the last three merged PRs?", "are these two attention items the same question?", "did this worker take an awkward path through cube that suggests a missing verb?". Those are judgment calls a threshold-based sweep answers badly in both directions (false closes are trust-destroying; false keeps make the feature pointless). The deterministic half — dead PIDs, expired leases, orphaned executions — is _already built_ as the sweep fleet; duplicating it buys nothing. The sweeps stay; Boothby adds only the judgment layer. (Where a candidate action turns out to be purely mechanical and safe — e.g. recovery-patch GC — the design does put it in engine code, not in the agent; see the action catalogue.)

### A2 — Model Boothby as an Automation (a standing instruction on the existing machinery)

Create a special automation whose standing instruction is "tidy the taxonomy", firing on the existing `automation_scheduler`.

Rejected. Automations are structurally wrong for this in five ways: they are **product-scoped** (Boothby is install-wide); their triage agents run **inside a cube workspace against a repo** (Boothby operates on the taxonomy and engine state, not a checkout); their only sanctioned write is **creating one task per fire** (`parse_triage_decision` markers) — Boothby needs a broad mutating verb surface; they run at **worker privilege** (Boothby needs coordinator-plus); and they have **no audit/undo model** beyond `automation_runs` outcomes. Bending the automation machinery to fit would contort both features. What _is_ reused: the adaptive-sleep + `kick` scheduler pattern, the `*_runs` history-table shape, and the decision-marker discipline for structured agent output.

### A3 — Fold groundskeeping into the coordinator session (Picard does it)

Give the existing coordinator session a standing instruction to tidy during idle periods.

Rejected. The coordinator is an interactive, human-paced session whose contract is planning and delegation; loading it with autonomous background mutations makes its behavior unpredictable exactly when the human is steering it, and it stops running when the human closes it — groundskeeping must happen precisely when nobody is looking. It also creates a self-reference problem for the secondary job: the coordinator mining its _own_ transcript mid-session. Finally, the audit requirement wants a single attributable actor identity; interleaving human-directed and autonomous actions in one session destroys that attribution.

### A4 — Journal in the CLI wrappers instead of the engine

Have Boothby call ordinary `boss`/`bossctl` verbs through a logging shim that records each invocation for the audit pane.

Rejected. CLI-side journaling is advisory: it records _intent_, not _effect_; it can't capture pre-images atomically with the mutation; it misses anything the agent does through a verb the shim doesn't wrap; and undo built on replaying CLI strings is fragile. The journal must live where the mutation happens — the engine's `WorkDb` layer — keyed on an authenticated actor identity, so it is complete and tamper-proof by construction (same reasoning that put editorial enforcement engine-side in `editorial_actions`).

## Chosen approach

Boothby is three cooperating pieces, all engine-owned:

1. **The Boothby runtime** (Rust, in-engine): a scheduler (timer + event triggers), a **pass brief composer** that assembles everything a pass needs to know, a **guarded action executor** that is the single choke point for every Boothby mutation (caps, guards, journal, pre-images), the **undo engine**, and the **findings ledger** (dedup + filing router).
2. **The Boothby session** (an unmodified `claude` agent): spawned per pass into a dedicated pane, coordinator-privileged plus transcript access, doing the judgment work and acting exclusively through `boss` / `bossctl` / `boss boothby` verbs.
3. **The Boothby tab in Agents** (SwiftUI): a dedicated tab beside the worker pools in the existing Agents view, holding his live pane, the audit feed with undo buttons, pass history, findings, and controls.

```text
            timer (adaptive)      engine events (debounced)     boss boothby run / UI
                   │                        │                            │
                   └────────────┬───────────┴────────────────────────────┘
                                ▼
                    ┌──────────────────────┐
                    │  Boothby scheduler    │  boothby_passes row opened
                    └──────────┬───────────┘
                               ▼
                    ┌──────────────────────┐
                    │  Pass brief composer  │  candidates from sweeps' leftovers,
                    │                      │  log cursors, transcript cursors,
                    └──────────┬───────────┘  findings ledger, prior journal
                               ▼
                    ┌──────────────────────┐   SpawnWorkerPane (boothby slot)
                    │  Boothby session      │──── reads brief, investigates via
                    │  (claude, own pane)   │     boss/bossctl read verbs + logs
                    └──────────┬───────────┘     + transcripts (elevated RPC)
                               │ mutating verbs only
                               ▼
                    ┌──────────────────────┐
                    │ Guarded action       │  caps • live-work guards • propose
                    │ executor (engine)    │  mode • journal + pre-image capture
                    └──────┬───────┬───────┘
                           │       │
              boothby_actions   effect (WorkDb mutation, reap,
              (audit journal)   chore/shake filing, cube verb)
                           │
                           ▼
              Boothby tab (in Agents): audit feed + undo  /  boss boothby undo <id>
```

### Activation model

**Timer.** A new `boothby_scheduler` loop wired in `server.rs::serve`, following `automation_scheduler.rs`'s adaptive pattern: sleep until the next due instant, clamped, woken early by a `kick` `Notify`. Cadence comes from the settings registry key `boothby.schedule` — a cron expression evaluated with the existing `automation_schedule.rs` cron/timezone math. **Default: every 30 minutes** (operator-decided; the pre-spawn `nothing_to_do` short-circuit is what makes a frequent cadence affordable — most fires conclude before a session is ever spawned). Groundskeeping is never urgent; a laptop that sleeps through fires simply catches up on the next wake — one pass, never a backlog, because passes are stateless-by-design and always evaluate _current_ state.

**Events.** Certain engine events indicate work worth a prompt visit rather than waiting for the timer. The event trigger subscribes engine-internally (a fan-out `DispatchEventSink` plus hooks at attention-item creation) — no polling. v1 trigger set, chosen because each represents a state a sweep flagged but could not fully resolve:

| Trigger                                                             | Source                                                                                                                                           |
| ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| Execution orphaned/reaped by a sweep                                | dispatch stages `dead_pid_reconcile`, `stale_worker_reconcile`, `orphan_active_redispatch`, `lost_workspace_reconcile`, `remote_lease_reconcile` |
| Worker parked by the nudge breaker                                  | attention kind `nudge_breaker_tripped`                                                                                                           |
| CI remediation exhausted / dead review                              | attention kinds `ci_remediation_exhausted`, `pr_review_died_without_findings`                                                                    |
| Spawn machinery unhealthy                                           | attention kind `app_spawn_capability_unhealthy`, stage `spawn_ack_timeout`                                                                       |
| SlotBusy / husk reconciliation fired                                | stages `husk_pane_reconcile`, `pool_claim_reconcile`                                                                                             |
| Boot after unclean shutdown, or `Unknown` liveness verdicts at boot | `engine-audit.log` start context + `run_reconcile.rs` verdicts                                                                                   |
| Engine error burst                                                  | ≥ N ERROR-level `engine-trace.jsonl` records within M minutes (counted by the existing metrics layer, not by tailing)                            |

Events are **debounced, not immediate**: a trigger arms a pass no sooner than `boothby.event_delay` (default 5 min, to let the sweeps' own churn guards settle) and passes are separated by a minimum gap (`boothby.min_pass_gap`, default 15 min). At most one pass runs at a time; triggers arriving mid-pass coalesce into a single follow-up.

**Manual.** `boss boothby run` and a Run-now button in the tab fire an immediate pass (still one-at-a-time).

### The Boothby session

**Spawn.** A new `WorkerKind::Boothby` through the existing spawn path (`runner.rs` → `spawn_flow.rs::start_worker` → `SpawnWorkerPane`), with these deltas:

- **No cube workspace.** Boothby's cwd is a stable engine-provisioned scratch directory, `<state_root>/boothby/home/` (mirroring how the coordinator session runs outside cube). His `.claude/CLAUDE.md` (the Boothby contract) and hook-wired `settings.json` are rendered there by `worker_setup.rs` with a new template.
- **Dedicated slot, not a pool.** One reserved pane slot in a new `boothby` slot namespace (`boothby-1`), hosted by the app inside the Boothby tab of the Agents view the way `workBossPanel` hosts the Picard pane — Boothby never competes for, or appears in, the worker/automation/review pools.
- **Fresh session per pass.** No `--resume` across passes. Continuity lives in durable state (journal, findings ledger, cursors, and a `<state_root>/boothby/memory.md` scratchpad he may update), not in conversation context. This keeps passes crash-tolerant, keeps context small, and makes every pass independently auditable. `session_id` + `transcript_path` are captured from his hooks onto the `boothby_passes` row exactly as `work_runs` does for workers.
- **Model/permission**: settings key `boothby.model` (default `opus`); permission wiring follows the answer-agent precedent (`answer_agent.rs`): a deny-by-default tool allowlist permitting `boss`, `bossctl`, `boss boothby`, `cube` _read_ verbs, log readers, and file reads under his scratch dir + the state root; file _writes_ only under his scratch dir; no `git`/`jj` mutation, no arbitrary shell.
- **Budget**: pass timeout (`boothby.pass_timeout`, default 15 min) enforced by the runtime — an overrunning session is stopped, the pass recorded `timed_out`, remaining candidates carried to the next brief.

**Pass lifecycle** (each row in `boothby_passes`): `trigger` (`schedule` | `event:<name>` | `manual`), `started_at`/`finished_at`, `outcome` (`completed` | `nothing_to_do` | `timed_out` | `failed` | `capped`), action/proposal/finding counts, `session_id`, `transcript_path`, and a one-paragraph agent-authored summary (via a `boss boothby pass-summary` verb, mirroring the automation decision-marker discipline).

**The pass brief.** The runtime composes `<state_root>/boothby/brief.json` before spawn so the agent starts oriented instead of spelunking: unresolved sweep leftovers (parked executions with age + fingerprint, `Unknown` verdicts, open worker-signal attention items), taxonomy candidates matching cheap SQL prefilters (tasks untouched > `boothby.stale_task_age`, projects with zero live tasks, attention groups > age threshold, near-duplicate titles), new log/transcript spans since the durable cursors, open findings and active suppressions, and the caps remaining for this pass. Prefilters only _nominate_; the agent decides.

### Action catalogue with reversibility classification

Every Boothby mutation flows through the guarded action executor and lands in `boothby_actions`. The catalogue is fixed in v1; each verb carries an autonomy default (`auto` = may act, `propose` = must file a proposal for operator approval) and a per-pass cap. Reversibility classes: **R** (fully reversible — pre-image restore), **S** (semi-reversible — a compensating action exists but isn't a true inverse), **I** (irreversible — audit only).

| #   | Action                                      | Mechanism (existing surface)                                                                                                              | Class | Undo                                                                                                                                  | Default                                                             | Cap/pass           |
| --- | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ----- | ------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------- | ------------------ |
| 1   | Close stale task                            | status → `archived`, `archived_reason` set, actor `boothby`                                                                               | R     | restore pre-image status + fields                                                                                                     | auto                                                                | 5                  |
| 2   | Close duplicate task                        | as #1 + `archived_reason: duplicate of T<n>` + description cross-link                                                                     | R     | restore pre-image                                                                                                                     | auto                                                                | 5 (shared with #1) |
| 3   | Dismiss stale attention group/member        | `dismiss_attention` (state → `dismissed`)                                                                                                 | R     | restore pre-image `state`/`answer_state`                                                                                              | auto                                                                | 5                  |
| 4   | Merge duplicate attentions                  | `AttentionMerge` fold (trigger value `boothby`), `retire_group_if_empty`                                                                  | R     | un-fold: restore member per pre-image, mark ledger row reversed (best-effort — `conflicted` fallback if the group was since actioned) | auto                                                                | 3                  |
| 5   | Resolve stale legacy `work_attention_items` | status → resolved                                                                                                                         | R     | restore `open`                                                                                                                        | auto                                                                | 5                  |
| 6   | Archive empty project                       | `ProjectStatus::Archived`, actor `boothby`                                                                                                | R     | restore pre-image status                                                                                                              | auto                                                                | 2                  |
| 7   | Prune abandoned revision rows               | soft-delete (`deleted_at`)                                                                                                                | R     | `restore_work_item`                                                                                                                   | auto                                                                | 5                  |
| 8   | Re-run effort heuristic on drifted rows     | `effort_level` update on rows where `effort_is_hand_set` is false, guided by `audit_effort::build_report`                                 | R     | restore pre-image `effort_level`                                                                                                      | auto                                                                | 10                 |
| 9   | File a chore (Boss product)                 | `create_chore` with `created_via = boothby:<finding-id>`                                                                                  | R     | soft-delete the chore                                                                                                                 | auto                                                                | 3                  |
| 10  | File a GitHub issue                         | `boss shake` path with label `boothby`, body sanitized                                                                                    | S     | close issue with a comment; ledger retains ref                                                                                        | auto                                                                | 3 (shared with #9) |
| 11  | Nudge a stalled review                      | create/refresh an attention item (`review_required`-style)                                                                                | R     | dismiss it                                                                                                                            | auto                                                                | 3                  |
| 12  | Reconcile PR-vs-task drift                  | status fix where `merge_poller` evidence is unambiguous (e.g. `in_review` task whose PR is long closed)                                   | R     | restore pre-image status                                                                                                              | propose                                                             | 3                  |
| 13  | Reap dead-but-live execution                | the `bossctl agents reap` path (exec → `orphaned`, slot released)                                                                         | I     | — (redispatch is the recovery)                                                                                                        | auto, only with two-pass confirmation of death (`confirm_two_pass`) | 3                  |
| 14  | Redispatch / unpark stuck work              | `bossctl work start`, nudge-breaker reset (new engine verb), probe/send                                                                   | S     | cancel the new execution                                                                                                              | auto                                                                | 3                  |
| 15  | Force-release a stuck cube lease            | `cube workspace force-release` (engine-mediated)                                                                                          | I     | —                                                                                                                                     | propose                                                             | 2                  |
| 16  | GC orphaned workspaces / consumed bookmarks | `cube workspace reconcile` / `cube workspace gc` (both support `--dry-run`; run dry first, act second)                                    | I     | —                                                                                                                                     | auto (they are conservative by construction)                        | 1 invocation       |
| 17  | Recovery-patch GC                           | delete `<state_root>/recovery/<exec-id>.patch` where the execution is terminal and older than `boothby.recovery_patch_ttl` (default 30 d) | I     | —                                                                                                                                     | auto                                                                | 20 files           |
| 18  | Cancel a ghost execution                    | `cancel_execution` on rows the sweeps flagged as unowned                                                                                  | S     | re-request execution                                                                                                                  | propose                                                             | 2                  |

A global cap (`boothby.max_actions_per_pass`, default 15) bounds total mutations regardless of per-verb caps; hitting any cap converts the remaining intended actions into proposals and marks the pass `capped`.

**Explicitly not in the catalogue** (evaluated, deferred): pausing/resuming global dispatch (`SetDispatchPaused` — too much blast radius for an agent; `future`), deleting products (no delete exists by design), acting on GitHub PRs, editing design docs, and modifying automations.

#### The stuck-state catalogue and what unwedging safely looks like

For each known stuck state, what detects it today, and Boothby's role:

- **Dead worker / dead lease.** Detected by `dead_pid_sweep` (registry PID probe with the T2450 corroboration guard), `dead_pane_sweep` (DB-persisted `shell_pid`, restart-robust), `remote_lease_reconcile` (remote probe), and `cube_lease_heartbeat` (TTL expiry + 3-strike auto-reap). These self-heal; Boothby's job is the _aftermath_: verify the redispatch actually happened, and where the same execution has died repeatedly (journal shows prior reaps), stop the loop and file a finding instead of a fourth redispatch.
- **Yellow idle-park.** `NudgeBreakerParked` (via `completion.rs::nudge_or_park`) and post-spawn `waiting_human` rows have **no auto-unpark today**. Boothby reads the park reason ("why yellow, and since when"), the worker's transcript tail, and either unparks (probe/redispatch, action #14) when the blocker has passed (e.g. the awaited CI finished), or files/refreshes an attention item so the human sees it — never blind-unparking a park that exists because the worker was flagging a real blocker.
- **Orphaned executions.** `orphan_sweep` redispatches `active` items with no live execution, but deliberately skips `waiting_human` and `pr_review` shapes, applies churn guards, and requires an idle slot. Boothby handles the residue: rows the sweep skipped for weeks, churn-guard-suppressed repeats (he decides: redispatch once more vs archive with a finding).
- **SlotBusy fallout.** `SlotBusy` is a hard per-spawn failure whose root causes (husk panes, leaked claims) are healed by `husk_pane_sweep` and `pool_claim_sweep`. A _recurring_ SlotBusy pattern on the same slot across passes is Boothby's cue to file a finding (it indicates an engine bug, not a transient) and, in propose mode, suggest `bossctl agents retire-pane`.
- **Doing-with-no-live-run.** Primary owner is `orphan_sweep` + boot-time `reconcile_active_dispatch`. Boothby covers the `Unknown`-verdict residue from `run_reconcile.rs` (today only `warn!`-logged): he inspects lease + transcript + workspace evidence and either reaps (#13, two-pass confirmed) or explicitly records "left alone: evidence of life".
- **Stuck leases / phantom-free workspaces.** Heartbeat and TTL handle the live path. Boothby runs `cube workspace reconcile --dry-run` / `gc --dry-run` and applies when the dry run is clean (#16), and proposes force-release (#15) for leases the heartbeat can't clear.

### Secondary job — log & transcript mining, filing

**Inputs, each with a durable cursor** (in `boothby_cursors`): `engine-trace.jsonl` + rotated segments (read via `boss-log-files`, cursor = segment id + byte offset), `engine-audit.log` (unclean shutdowns, crash loops), dispatch-events JSONL (`stage_stalled`, `spawn_ack_timeout`, error-outcome clusters), and transcripts. Worker transcripts are enumerated from `work_runs.transcript_path`; **coordinator transcripts** are discovered by the runtime (scan `~/.claude/projects/<slug>/` for the coordinator's cwd slug, newest sessions first) and exposed to the session only through the elevated transcript RPC — this is the concrete "power the ordinary coordinator lacks."

**What he looks for**: repeated ERROR signatures (clustered by the `transient_error.rs` classification where applicable), anomalies (a sweep firing every interval, dispatch stages with rising latency, IPC error clusters), and **friction**: long retry chains against `cube`/`jj`/`gh`, workers repeatedly hand-rolling the same multi-step maneuver, `[effort-escalation]` patterns (cross-checked with `boss product audit-effort`), coordinator sessions spending many turns on something one verb should do.

**Filing rules.**

- **Routing**: if the settings key `coordinator.direct_developer_mode` is true, _or_ the local taxonomy has a product whose `repo_remote_url` matches the Boss upstream (`spinyfin/mono`) — i.e. this is a Boss developer machine — file a **chore against that product**. Otherwise file a **GitHub issue via the `boss shake` path** (the embedded GitHub App), labeled `boothby`.
- **Dedup — three layers**: (1) the **findings ledger**: every observation gets a stable `fingerprint` (hash of finding kind + normalized subject, e.g. error signature or friction pattern id); a fingerprint that already has a `filed` or `suppressed` ledger row is never re-filed, only its `last_seen`/`occurrences` updated. (2) a **durable marker on what he files**: chores carry `created_via = boothby:<finding-id>` (the `is_known_created_via` prefix mechanism); issues carry the `boothby` label and a `boothby-finding: <id>` body trailer. (3) a **search-before-file step** inside the filing verb: the engine checks open chores by `created_via` prefix and open `boothby`-labeled issues (title match) before creating anything — the belt-and-suspenders against a lost ledger.
- A finding whose filed chore/issue gets closed by a human without a fix flips to `suppressed` (cooldown forever unless the operator clears it) — closing Boothby's issue _is_ the veto signal.

### Audit & undo data model

Durable engine state (SQLite migrations in `work/migrations_*.rs`), because the UI reads it and undo depends on it — logs are not the audit trail.

```sql
CREATE TABLE boothby_passes (
    id            TEXT PRIMARY KEY,            -- bp_<ts>_<n>
    trigger       TEXT NOT NULL,               -- 'schedule' | 'event:<name>' | 'manual'
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    outcome       TEXT,                        -- completed | nothing_to_do | timed_out | failed | capped
    actions_count INTEGER NOT NULL DEFAULT 0,
    proposals_count INTEGER NOT NULL DEFAULT 0,
    findings_count  INTEGER NOT NULL DEFAULT 0,
    summary       TEXT,                        -- agent-authored, via pass-summary verb
    session_id    TEXT,
    transcript_path TEXT
);

CREATE TABLE boothby_actions (
    id            TEXT PRIMARY KEY,            -- ba_<ts>_<n>
    pass_id       TEXT NOT NULL REFERENCES boothby_passes(id),
    seq           INTEGER NOT NULL,
    verb          TEXT NOT NULL,               -- catalogue slug, e.g. 'close_stale_task'
    target_kind   TEXT NOT NULL,               -- task | project | attention | attention_item | execution | lease | workspace | file | issue
    target_id     TEXT NOT NULL,
    params        TEXT,                        -- JSON: verb inputs
    rationale     TEXT NOT NULL,               -- agent-supplied one-liner, required
    pre_image     TEXT,                        -- JSON of mutated fields before (NULL for I-class)
    post_image    TEXT,                        -- JSON after, for undo conflict detection
    reversibility TEXT NOT NULL,               -- 'reversible' | 'semi' | 'irreversible'
    undo_state    TEXT NOT NULL DEFAULT 'none',-- none | undoable | undone | expired | conflicted
    undone_at     TEXT,
    undone_by     TEXT,                        -- 'human' (undo is human-only)
    created_at    TEXT NOT NULL
);
CREATE INDEX boothby_actions_by_pass ON boothby_actions(pass_id, seq);
CREATE INDEX boothby_actions_by_target ON boothby_actions(target_kind, target_id);

CREATE TABLE boothby_findings (
    id            TEXT PRIMARY KEY,            -- bf_<ts>_<n>
    fingerprint   TEXT NOT NULL UNIQUE,
    kind          TEXT NOT NULL,               -- error | anomaly | perf | friction | taxonomy
    subject       TEXT NOT NULL,               -- JSON refs: log span / transcript span / row ids
    first_seen    TEXT NOT NULL,
    last_seen     TEXT NOT NULL,
    occurrences   INTEGER NOT NULL DEFAULT 1,
    status        TEXT NOT NULL,               -- open | filed | resolved | suppressed
    filed_kind    TEXT,                        -- chore | github_issue
    filed_ref     TEXT,                        -- task id or issue URL
    suppressed_reason TEXT
);

CREATE TABLE boothby_cursors (
    source        TEXT PRIMARY KEY,            -- e.g. 'engine-trace', 'dispatch-events', 'transcript:<session>'
    position      TEXT NOT NULL,               -- JSON: segment/offset or timestamp high-water mark
    updated_at    TEXT NOT NULL
);
```

**How the journal is written.** The executor captures the pre-image _in the same transaction_ as the mutation: for taxonomy verbs it snapshots exactly the columns it will touch (shape borrowed from `project_property_audit`), applies via the existing actor-attributed path (`update_work_item_as_actor(id, patch, "boothby")` — `boothby` becomes a fourth `last_status_actor` value beside `human|boss|engine`), writes the `boothby_actions` row, and only then commits. I-class actions journal `params` + evidence in `rationale` with `pre_image = NULL`.

**How undo works.** Human-only (app button or `boss boothby undo <action-id>`; the Boothby session itself has no undo verb — his correction path is a new forward action, which prevents self-laundering of mistakes). The undo engine loads the action, compares current row state against `post_image`: on match, applies `pre_image` through the same actor-attributed mutation layer (actor `human`, and the original action flips `undo_state = undone`); on mismatch (someone changed the row since), marks `conflicted` and the UI shows both images for a manual decision — undo never silently overwrites later changes. Undoing a filing = soft-delete the chore / close the issue with an explanatory comment. Every undo also **auto-suppresses the action's fingerprint** so Boothby never redoes what a human undid.

**Retention.** Passes and actions are pruned after `boothby.retention_days` (default 90) by a small sweep (same pattern as `execution_retention_sweep.rs`); an action's `undo_state` flips to `expired` when its target row is gone or retention lapses. Findings live indefinitely (they are the dedup memory); `resolved` findings compact after 180 days.

### Safety rails

- **Modes** (settings key `boothby.mode`): `off` | `propose` | `auto`. **Ships defaulting to `propose`**: every intended action becomes a proposal (an attention group of kind `question`, one `yes_no` member per action, anchored to the audit pane) and nothing mutates until the operator approves — approval executes through the same executor, so the journal is identical. `auto` honors the per-verb autonomy column; verbs marked `propose` in the catalogue stay propose-gated even in `auto`. The operator flips modes per install; a **trust ratchet** (auto-promote a verb to `auto` after N consecutive approvals) is `future`.
- **Blast-radius caps** are enforced in the executor (not the prompt): per-verb caps and the global per-pass cap above; a capped pass ends with proposals, never silent drops.
- **Never fight live work.** The executor rejects mutations targeting: rows with a non-terminal execution or a claimed pool slot; rows whose cube lease is currently held; rows a human touched within `boothby.human_touch_cooldown` (default 72 h, via `updated_at` + `last_status_actor`); and anything the coordinator session has an in-flight probe against. I-class operational verbs additionally require the same `confirm_two_pass` double-read the husk/terminal sweeps use.
- **Human veto is permanent.** If a human reverses a Boothby action (undo, reopen, unarchive, restore — detected as a human-actor status change on a row with a journaled action), the fingerprint is suppressed; Boothby may mention it in a pass summary but never re-acts without the operator clearing the suppression (`boss boothby suppressions clear <id>`).
- **Install-root scoping.** All destructive verbs resolve targets through the engine's own `WorkDb` and its own cube install; the executor refuses path-shaped targets outside `<state_root>`/the cube data dir. Test instances (`BOSS_DB_PATH` overrides, per `test-instance-isolation.md`) get their own Boothby state and never see the production one.
- **Kill switch.** `boothby.mode = off` (settings), `boss boothby disable`, and a toggle in the tab all stop the scheduler; an in-flight pass is stopped at the next executor call. Independent of, and unaffected by, `bossctl dispatch pause`.

### Privilege model

- **Identity.** The engine spawns Boothby and registers his pane's `shell_pid` as a third trust root, `boothby_pid`, beside `app_pid` and `boss_pid` — engine-internal (no `RegisterBossSession`-style RPC; the engine already knows the pid it spawned). `authorize_rpc` gains: descendants of `boothby_pid` pass `User` and `AppOrBoss` tiers (so ordinary `boss` verbs and the operational `bossctl` verbs work), are **excluded from the worker-pane denylist** (his pane is registered for hook correlation but must not be classed as a worker), and pass a new **`BoothbyOrApp`** tier gating the elevated surface.
- **Elevated surface (`BoothbyOrApp` tier)**: the transcript-access RPCs (`ListAgentSessions`, `ReadTranscriptSegments` — worker _and coordinator_ scopes, served engine-side so file ACLs never loosen), the Boothby action verbs (`boothby.act`, `boothby.pass-summary`, `boothby.file-finding`), and cursor read/write. The app shares the tier so the tab can render everything. The ordinary coordinator does **not** gain transcript access — that asymmetry is deliberate and is what "Final Boss" means in practice.
- **Coordinator-transcript mining is on by default** once Boothby is enabled (operator-decided): there is no separate opt-in setting. Enabling Boothby _is_ the consent to mine the coordinator's transcripts; the containment rules below govern where that material may flow, and disabling Boothby (`boothby.mode = off`) stops the mining with everything else.
- **Boothby never gains**: engine shutdown (token-gated, unchanged), `RegisterBossSession`, editorial-rule editing, settings writes (operator-only), or undo of his own actions.
- **Containment / no leaks downward.** Boothby's _reads_ include coordinator-only material; his _writes_ land in worker-visible and GitHub-visible surfaces (chore descriptions, issue bodies, attention text). The filing verb therefore sanitizes outbound bodies: references to transcripts are by `(session_id, span)` pointer — resolvable only through the elevated RPC — never inlined excerpts beyond a short quoted snippet that the sanitizer strips of env values, tokens, and absolute home paths; GitHub-bound bodies additionally pass the existing editorial-rules layer. Worker sessions never see `boothby`-tier RPCs (peer-pid ancestry), and Boothby's scratch dir lives under the state root, which worker sandboxes already cannot read.

### Idempotence & convergence

- **Fingerprint everything.** Actions and findings both carry stable fingerprints. Before acting, the executor consults the journal: a fingerprint with a prior `undone`/`conflicted` action or an active suppression is refused (surfaced as a proposal at most). This makes close→reopen→close structurally impossible rather than behaviorally unlikely.
- **Age gates.** Staleness verbs only nominate rows beyond thresholds (`boothby.stale_task_age` default 30 d untouched; attention groups 14 d; empty projects 7 d), so a wakeup burst can't act on fresh state.
- **Cursors, not re-scans.** Log/transcript mining advances durable cursors; a crash mid-pass re-reads at most one span twice, and the findings ledger's fingerprint unique-constraint makes the second read a no-op.
- **Passes are stateless.** Each pass evaluates current state; there is no queued backlog of stale intents that could fire after the world changed (the same reasoning as the automation scheduler's skip-if-stale rule).

### Interaction with the existing engine

The engine keeps owning reconciliation; Boothby is a privileged _client_ of it, plus a scheduler the engine also owns. Concretely: sweeps keep their intervals and their verbs; the executor reuses their primitives (`confirm_two_pass`, the reap path, `request_execution_with_live_check`) instead of reimplementing them; `reconcile_audit.rs` description lines gain a `[boothby]` variant so row-level history stays human-readable in place; dispatch events gain `boothby_pass` / `boothby_action` stages so the existing Activity log shows him too; and the UI stays a thin client — every pane below renders engine RPC responses and topic pushes (`boothby.activity` topic), computing nothing itself.

### UI surface

Boothby lives **inside the existing Agents view, as his own tab** — not a new top-level tab. `AgentPoolKind` (`WorkersDetailView.swift`) gains a `boothby` case, so the segmented pool picker reads Bridge Crew / Lower Decks / Automations / Reviewers / **Boothby**. Like the other pool pages, the Boothby page stays permanently in the `ZStack` hierarchy with opacity/hit-testing switching, so his libghostty surface survives tab switches. Unlike them, its content is not a `WorkerGrid` but a single dedicated panel:

- **Left column**: pass history (rows: trigger icon, outcome badge, counts, relative time — the `AutomationRunRow` shape), plus a findings section with status chips.
- **Main column, default**: the **audit feed** — a freshest-first timeline of `boothby_actions` modeled on `ActivityLogView`'s `ActivityRow` (headline = verb + target short-id, subline = rationale, outcome color), each row expandable to pre/post images, with an **Undo** button on `undoable` rows using the `PlannerUndoButton` confirmation-dialog pattern (and a "Conflicted — review" state showing both images). Selecting a pass filters the feed to it and links to his transcript in the transcript viewer.
- **Main column, live**: Boothby's terminal pane (the `workBossPanel`/`BossPaneModel` hosting pattern), shown while a pass runs — collapsed to a status strip when idle.
- **Header strip** (below the pool picker): mode picker (`off`/`propose`/`auto`), Run now, next-due time, and a proposals badge deep-linking to the Notifications window (proposals ride the existing `AttentionsView` yes/no cards — approve/deny is one click there).

### CLI surface

```
boss boothby status                         # mode, next fire, last pass, counts
boss boothby run                            # fire a pass now
boss boothby enable | disable               # flip boothby.mode
boss boothby mode <off|propose|auto>
boss boothby passes [--json]                # pass history
boss boothby actions [--pass <id>] [--json] # audit journal
boss boothby undo <action-id> [--force]     # human undo (--force = accept conflicted)
boss boothby findings [--status <s>]
boss boothby suppressions [clear <id>]
```

(Internal verbs used by the session itself — `act`, `pass-summary`, `file-finding` — are `boothby`-tier and hidden from help.)

### Observability of Boothby

His pass history and journal are first-class UI/CLI surfaces (above); his session transcripts are recorded per pass and open in the transcript viewer; his runtime logs under `tracing` target `boothby` land in `engine-trace.jsonl` (filterable in the Activity window); dispatch-event stages make his passes visible in the existing Activity feed; and `metrics_counter` gains `boothby.*` counters (passes, actions by verb, proposals, undos, suppressions) visible in the Metrics viewer. Debugging Boothby is therefore the same motion as debugging any worker, plus his journal.

## Risks / open questions

1. **Judgment quality on closes.** Even propose-first, a stream of bad proposals erodes trust and wastes operator time. Mitigations: conservative age gates, the brief's prefilters, rationale-required actions. Reviewer: are the default thresholds (30 d task staleness, caps of ~5/pass) the right opening posture, or should v1 be stricter?
2. **`last_status_actor` widening — resolved: new `boothby` actor value.** The operator confirmed adding `boothby` as a fourth `last_status_actor` value beside `human|boss|engine` (exact attribution; human-veto detection keys on actor identity), over the cheaper alternative of reusing `engine` and leaning on the journal. Residual risk: the value set is consumed in several places, so the exhaustive-match audit of consumers — cheap but easy to fumble — is carried explicitly by task 1 of the breakdown.
3. **Coordinator-transcript discovery is heuristic.** With no engine-held pointer to the coordinator's session, discovery-by-cwd-slug can miss sessions (nonstandard launch dirs) or over-match. A follow-up that records the coordinator's `session_id` at `RegisterBossSession` time would make it exact; v1 ships the heuristic plus that chore.
4. **Two attention systems.** Boothby touches both the modern groups (`attentions.rs`) and legacy `work_attention_items`. If the planned unification (`unify-work-item-kinds-flavors.md` era) lands mid-build, catalogue verbs #3–5 need re-pointing. Kept as three separate small verbs to make that re-point cheap.
5. **Undo of attention merges — resolved: best-effort undo accepted.** Merge-undo restores the folded member but cannot un-ring group retirement side effects in every corner (e.g. an actioned group). The operator accepted classifying merge-undo as best-effort R with a `conflicted` fallback; merges stay `auto` rather than being demoted to `propose`.
6. **Event-trigger coupling.** Subscribing Boothby to dispatch stages couples him to an enum that grows freely. The fan-out sink must treat unknown stages as no-ops so engine PRs never break on him.
7. **Cost.** The operator chose a 30-minute cadence over the drafted 2 hours, which makes opus-pass spend a live concern. The load-bearing mitigation is the `nothing_to_do` short-circuit: the brief composer concludes _before spawn_ that no candidates exist and skips the session entirely (recorded as a pass with no session), so an idle install pays a few SQL prefilters per fire, not an opus session.
8. **Shake-side dedup durability.** If `state.db` is lost, the search-before-file step (labels + `created_via`) is the only dedup layer left. Accepted: it exists precisely for this.

## Proposed implementation task breakdown

Entries are one reviewable PR by one worker in one session. Closely-coupled steps are coalesced into single entries (several are therefore `large`); an entry that lands in one subsystem in one sitting beats two half-entries that co-edit the same files.

1. **Boothby schema, protocol types + pre-image capture** — Migrations for `boothby_passes`, `boothby_actions`, `boothby_findings`, `boothby_cursors`; `bon::Builder` protocol structs; the `boothby` `last_status_actor` constant + exhaustive-match audit; `created_via` prefix `boothby:`; extend the mutation layer (`updates.rs`/`workitems.rs`/`attentions.rs`) so actor-`boothby` mutations capture pre/post images of touched columns in-transaction and append `boothby_actions` rows, with no behavior change for other actors. Effort: `large`. Depends on: none.
2. **Guarded action executor + undo engine** — One subsystem, built with its inverse: the verb catalogue table (autonomy, caps, reversibility), per-pass cap accounting, live-work/human-touch/lease guards, `confirm_two_pass` for I-class verbs, fingerprint refusal, journal writes for non-WorkDb effects, `boothby.act` RPC; plus conflict-checked pre-image restore, `undo_state` lifecycle, fingerprint auto-suppression on undo/human-veto detection, `UndoBoothbyAction` RPC, and the retention prune of passes/actions. Effort: `large`. Depends on: 1.
3. **Scheduler, event triggers + pass lifecycle** — `boothby_scheduler` loop (adaptive sleep + kick), settings keys (`boothby.mode/schedule/caps/…`), `boothby_passes` lifecycle, `nothing_to_do` pre-spawn short-circuit, `ListBoothbyPasses`/`GetBoothbyState`/`RunBoothbyPass`/`SetBoothbyMode` RPCs + `boothby.activity` topic; the fan-out `DispatchEventSink` + attention-creation hooks feeding the kick with coalescing, `event_delay`, `min_pass_gap` (unknown stages are no-ops). Effort: `medium`. Depends on: 1. (Parallel with 2.)
4. **Pass brief composer** — SQL prefilters (stale tasks, empty projects, aged attentions, near-dup titles), sweep-leftover collection (parked rows, `Unknown` verdicts, open worker-signal attentions), caps-remaining snapshot, brief.json rendering. Effort: `medium`. Depends on: 3.
5. **Session spawn + privilege tier and transcript access** — The spawn and the trust root it registers land together: `WorkerKind::Boothby`, `<state_root>/boothby/home/` provisioning, contract `CLAUDE.md` + allowlist settings template (answer-agent pattern), dedicated `boothby-1` slot via `SpawnWorkerPane`, session/transcript capture onto the pass, pass timeout, `pass-summary` verb; `boothby_pid` trust root + `BoothbyOrApp` tier in `authorize_rpc` (incl. worker-denylist exclusion), `ListAgentSessions`/`ReadTranscriptSegments` RPCs, coordinator-session discovery heuristic + the exact-pointer chore (record coordinator `session_id` at `RegisterBossSession`). Effort: `large`. Depends on: 3, 4.
6. **Log & transcript mining inputs** — Both halves feed the same brief sections and share the cursor machinery: cursored readers over `engine-trace.jsonl` (+ rotation), `engine-audit.log`, dispatch-events JSONL, and the error-burst counter for the trigger; worker/coordinator transcript enumeration + cursors, friction heuristics (retry chains, escalation patterns, audit-effort cross-check); extracted spans summarized into the brief. Effort: `large`. Depends on: 4, 5 (the log half can start once 4 merges; the transcript half needs 5's RPCs).
7. **Findings ledger + filing router** — Fingerprint upsert/dedup, `file-finding` verb with routing (`direct_developer_mode` OR Boss-product presence → chore with `created_via`; else shake path with `boothby` label + finding trailer), search-before-file, outbound sanitizer + editorial pass, suppression on human-close. Effort: `medium`. Depends on: 2. (Parallel with 3–6.)
8. **Grounds-keeping verb set** — Executor wiring for catalogue #1–8, #11–12 (taxonomy verbs): archived-reason conventions, duplicate cross-links, attention dismiss/merge/resolve with pre-images, effort re-run, PR-drift fix, review nudge. Effort: `medium`. Depends on: 2. (Parallel with 7; distinct files.)
9. **Operational verb set: unwedging + hygiene GC** — Executor wiring for catalogue #13–18: reap path reuse, redispatch/unpark (incl. nudge-breaker reset verb), engine-mediated cube force-release, ghost-execution cancel, two-pass confirmation throughout; dry-run-then-apply `cube workspace reconcile`/`gc`, recovery-patch GC with TTL. Effort: `medium`. Depends on: 2. (Parallel with 8; touches coordinator/sweep call sites, not 8's files.)
10. **Proposal flow + operator CLI** — The operator-facing control surface: propose-mode conversion of intended actions into attention groups (yes/no members), approve→execute through the executor, deny→suppress, proposals badge data; the full `boss boothby` noun (`status/run/enable/disable/mode/passes/actions/undo/findings/suppressions`, `--json` via `print_entity`). Effort: `large`. Depends on: 3, 8 (first consumers; verbs from 9 join for free).
11. **App: the Boothby tab in Agents** — One entry because the three natural slices co-edit the same view files and would have to be serialized anyway: `AgentPoolKind.boothby` + pool-picker entry, pass-history/findings column, `ActivityRow`-style audit feed with pre/post-image expansion, `PlannerUndoButton`-pattern undo with confirmation + conflicted-state review, header controls (mode picker, Run now, proposals badge → Notifications deep-link), hosting the live `boothby-1` pane (`BossPaneModel` pattern) with transcript-viewer link, topic subscription. Effort: `large`. Depends on: 2, 3 (RPCs, undo states), 5 (pane hosting — feed and history render before 5 lands).
12. **End-to-end validation pass** — Seeded test-instance fixture (stale tasks, dup attentions, parked execution, dead-pid execution, log error burst); drive a propose-mode and an auto-mode pass; assert journal completeness, undo round-trips, fingerprint dedup, cap enforcement, veto suppression. Effort: `medium`. Depends on: 8, 9, 10 (engine surface complete; app not required).

**Deferred (`future`, not v1 blockers)** — kept out of the numbered breakdown: the trust ratchet (auto-promotion of verbs after N approvals; builds on 10), remote-host awareness (Boothby acting on remote executions/leases; builds on 9), metrics-anomaly detection (statistical baselines over `metrics_counter`; builds on 6), and dispatch pause/resume as a Boothby verb (builds on 9; deliberately excluded from v1 blast radius).

Parallelism at a glance: after 1 merges, {2, 3} run in parallel; after 2, {7, 8, 9} fan out (distinct subsystems/files); {4 → 5 → 6} chain under 3; 10 follows 8; the app track (11) runs parallel to the engine tracks once 2, 3, and 5 land; 12 is the integration gate before enabling `propose` mode by default.
