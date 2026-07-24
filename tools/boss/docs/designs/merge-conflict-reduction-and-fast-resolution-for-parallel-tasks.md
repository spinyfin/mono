# Boss: Merge-Conflict Reduction and Fast Resolution for Parallel Tasks

- **Status:** Implemented — all 15 project tasks done (design #1791 merged 2026-07-13; implementation complete 2026-07-14). **[Postmortem](#postmortem-2026-07-19) added 2026-07-19**, covering as-built reality, decisions that diverged, and the six-day production-hardening follow-up family. (The P2264 project row still read `planned` at postmortem time — stale bookkeeping, corrected via the postmortem's follow-ups; every task is in fact done.) A follow-up review pass (2026-07-21, this doc's only merged input since the postmortem was PR #2154 — the postmortem PR itself) spot-checked the postmortem's PR citations against the current PR record: one citation (the Era-A trigger incident's PR number) did not check out and is corrected in place; the T4 row's scope (rung 1 only, not rung 0+1) is corrected in place. No other divergence found; the rest of the postmortem's claims held up.
- **Project:** `Merge-conflict reduction and fast resolution for parallel tasks` (proj 2264).
- **Author:** Boss worker (`exec_18bf099316577698_110`).
- **Related:** [`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) (detection + resolution of in-review conflicts — the path this doc makes faster), [`auto-rebase-stacked-prs.md`](auto-rebase-stacked-prs.md) (the engine-direct mechanical-rebase tier we extend), [`maintenance-tasks.md`](maintenance-tasks.md) (the Automations mechanism direction 1 reuses), [`engine-counter-metrics-framework.md`](engine-counter-metrics-framework.md) (counters), and the [incident-002 postmortem](../postmortems/incident-002-merge-conflict-deletion-blessed-by-review.md) whose remediations (queued as **T2253**) are the safety gates this doc must compose with, not weaken.

---

## Problem

Operator framing, verbatim:

> "Merge conflicts are a significant problem in boss — almost every task is running into a merge conflict. I'm thinking this is likely to be a problem in any codebase of reasonable size. I think it'd be neat if boss had a better solution for it."

Boss dispatches up to eight worker tasks in parallel against one repository (`MAX_WORKER_POOL_SIZE = 8`, `coordinator.rs:82`). Each task branches from `main`, works for minutes-to-tens-of-minutes, and opens a PR. While it works, `main` moves under it — other siblings merge. The result is a steady stream of merge conflicts, and today each one is expensive: the engine detects the conflict (`conflict_watch::on_conflict_detected`), spawns a full `kind=revision` worker, and that worker spins up a fresh Claude session, reads a long brief, leases a workspace, checks out the PR branch, rebases, resolves by hand, pushes, and comments. That is a whole agent-lifecycle to fix what is often a lockfile.

Three distinct cost centers hide inside "merge conflicts are a problem," and they want different fixes:

1. **Frequency** — how often two in-flight tasks collide at all. Driven by _structural_ collision (many tasks edit the same hot files) and _temporal_ collision (they are in flight at the same time).
2. **Resolution cost/latency** — how long and how expensive it is to clear a conflict once it happens. Today: one full worker per conflict, no matter how trivial.
3. **Safety** — resolution must not silently delete merged work. Incident-002 is the proof this is real; its fixes are queued as T2253. **This project owns frequency and speed, not the safety gates** — but every speed change here must pass through those gates unchanged (see [Composition with T2253](#composition-with-t2253-the-safety-gates)).

This document establishes real conflict telemetry as a first-class deliverable, then proposes a layered program that attacks each cost center with the cheapest lever that works, and that keeps the operator's hard constraint front-and-center: **high parallelism is non-negotiable — any proposal that wins by serializing loses.**

### Evidence

**Rate (a known undercount).** PRs carrying the "🤖 boss resolved merge conflicts" bot comment: `spinyfin/mono` 132 all-time, **23 of 194 PRs (~12%)** since 2026-06-20; `brianduff/flunge` **8 of 88 (~9%)**. This only counts conflicts that surfaced as a bot-comment resolution on an _open_ PR. It misses (a) conflicts a producing worker resolves _before_ opening its PR (the `cube workspace rebase` it runs during normal work), and (b) any resolution path that does not comment. Operator experience — "almost every task" — is consistent with the true rate being materially higher than 12%. **We do not actually know the rate.** Fixing that is Layer 0 of the design.

**Hotspots (what actually conflicts).** Aggregating the changed-file lists of the 70 most-recent conflicted PRs (777 file-rows) gives a direct read on the conflict surface:

| File                                   | Appears in N conflicted PRs |
| -------------------------------------- | --------------------------- |
| `MODULE.bazel.lock`                    | 17                          |
| `Cargo.lock`                           | 15                          |
| `engine/core/src/completion.rs`        | 14                          |
| `engine/core/src/coordinator.rs`       | 13                          |
| `tools/checkleft/src/main.rs`          | 12                          |
| `engine/core/src/app.rs`               | 12                          |
| `engine/core/src/merge_poller.rs`      | 11                          |
| `protocol/src/types.rs`                | 10                          |
| `engine/core/src/work/mappers.rs`      | 9                           |
| `engine/core/src/runner.rs`            | 9                           |
| `protocol/src/wire.rs`                 | 8                           |
| `engine/core/src/work/schema_init.rs`  | 8                           |
| `engine/core/src/work/migrations_b.rs` | 7                           |

By category (share of 777 file-rows in conflicted PRs):

| Category                                                      | Rows | Nature                                         |
| ------------------------------------------------------------- | ---- | ---------------------------------------------- |
| **Lockfiles** (`Cargo.lock`, `MODULE.bazel.lock`, `*.lock`)   | 34   | **Mechanically regenerable — no agent needed** |
| **`BUILD.bazel`** files                                       | 28   | Largely mechanical (target/dep lists)          |
| `mod.rs` / `lib.rs` registries                                | 57   | Append/registry — often union-mergeable        |
| Protocol `types.rs` / `wire.rs`                               | 18   | Append-heavy structs/enums                     |
| Engine hot files (`completion`/`runner`/`coordinator`/`work`) | 43   | Genuinely semantic — need judgment             |
| Schema / migration append-points                              | 16   | Append-only, ordering-sensitive                |
| Test files                                                    | 83   | Mixed                                          |

Cross-checking against raw churn over the last 500 `main` commits (independent signal) puts the same files on top: `MODULE.bazel.lock` (53), `completion.rs` (52), `runner.rs` (49), `coordinator.rs` (44), `Cargo.lock` (37), `types.rs` (35), `merge_poller.rs` (35).

Three things fall out of this evidence and shape the whole design:

- **The single most common conflict surface is formulaic files, and in _this_ repo the top instances are lockfiles.** `MODULE.bazel.lock` and `Cargo.lock` are #1 and #2, and their correct resolution is _formulaic_: take both sides' manifest changes and regenerate. But the design conclusion is one level up from the instances: the engine must stay **agnostic to any particular repo's tooling choices**, so the deliverable is a _general deterministic-resolver feature_ — a registry of pluggable per-file-class resolvers — not engine logic hardcoded to two filenames. Lockfile resolvers are merely the first built-ins because the data says they are the highest-value targets by a wide margin (lockfiles + `BUILD.bazel` alone are ~8% of conflicted file-rows and need zero agent judgment). This is the operator's direction-5 example.
- **A large fraction of the rest is append/registry-shaped** (`mod.rs`/`lib.rs` registries 57 rows, protocol types 18, migrations 16). This is the direction-1 refactoring target: append-only structures → keyed maps, hotspot files split, registries extracted. Precedent already exists in this repo — `protocol/src/types.rs` adopted `#[derive(bon::Builder)]` _specifically_ so additive-change PRs stop touching every construction site (see the builder-pattern convention in `CLAUDE.md`). That is exactly the class of refactor direction 1 should schedule automatically.
- **Only ~40 rows are genuinely semantic** engine hot files that actually need an agent. Everything else is mechanical or structural. **Today all of it goes to a full worker.** That is the waste.

---

## Framing: three cost centers, one escalation ladder

The design is organized around the three cost centers, but its centerpiece is a single idea for cost center #2: **a resolution escalation ladder.** Every detected conflict enters at the bottom rung and climbs in exactly two situations: the cheaper rung **declines** (it cannot produce a resolution), or the cheaper rung's completed resolution is **rejected by post-resolution review** (the tripwire, the build gate, or an AI review agent judges it wrong — see "escalation on review rejection" under rung 3 below). Each rung is strictly cheaper than the one above it:

```
Rung 0  Deterministic resolver        free, instant, no agent      ← direction 5
        (lockfile regenerate, union-merge, reformat)
Rung 1  Engine-direct mechanical/      seconds, no agent            ← directions 3 & 4
        structural jj rebase
        (cube workspace rebase; jj auto-resolves non-overlapping hunks)
Rung 2  Small focused resolution       fast, cheap, pre-staged      ← direction 3
        agent (bounded prompt, cheap model, starts AT the conflict)
Rung 3  Full worker (today's path)     slow, expensive              ← unchanged fallback
```

The ladder's key property: **speed is bought by resolving as low as possible, and safety is unchanged because the T2253 gates run on the _result_, independent of which rung produced it.** A lockfile regenerated by rung 0 and a hand-merge by rung 3 both pass through the same both-parents deletion tripwire and the same build gate. We are changing _who_ resolves, never _what gets checked afterward_.

The other cost centers wrap around the ladder:

- **Cost center #1 (frequency)** is attacked two ways: _structurally_ by a telemetry-driven refactoring automation that incrementally de-hotspots the repo (direction 1), and _temporally_ by conflict-aware scheduling that sequences merges of likely-overlapping siblings without serializing their dispatch (direction 2).
- **Cost center #3 (safety)** is not owned here; we compose with T2253.
- **Measurement (Layer 0)** underpins all of it — telemetry tells us the true rate, which rungs fire how often, and which files to refactor next.

---

## Goals

- **Measure the real conflict rate and surface.** Close the telemetry coverage gap so we know the true frequency, the per-file/per-file-pair hotspots, and the per-class breakdown (mechanical vs. semantic). Measurement is a deliverable, not an afterthought.
- **Make the common conflict free.** Resolve formulaic conflict classes with a general, extensible deterministic-resolver feature — no agent, instant, zero token cost. The engine core stays agnostic to repo tooling; individual resolvers (lockfiles first, per the data) are pluggable built-ins today and a user-extension point later.
- **Make the mechanical conflict agent-free.** Attempt an engine-direct structural rebase before ever spawning an agent, exactly as `auto_rebase` already does for stacked PRs; auto-retire when jj's structural merge resolves it.
- **Make the semantic conflict cheap and fast.** When an agent is genuinely required, spawn a _small, focused, pre-staged_ resolver (cheap model, tight prompt, workspace already at the conflict) rather than a full worker that starts from a cold session.
- **Reduce frequency without reducing parallelism.** Incrementally de-hotspot the repo via a standing automation, and sequence merges of likely-overlapping siblings with a _non-blocking_ mechanism that never gates dispatch.
- **Generalize.** Nothing here may overfit to mono/flunge. The resolver registry, the telemetry, the automation, and the scheduling seam are all repo-agnostic; only individual resolvers (e.g. "Cargo.lock") are ecosystem-specific, and they are pluggable.
- **Compose with T2253, never weaken it.** Every faster/lighter path routes its result through the same safety gates.

## Non-goals

- **The safety gates themselves.** The both-parents deletion tripwire, the preservation-rule brief, the supersedes-citation check, the removal-forward comment (incident-002 P1–P4, queued as T2253) are out of scope here. We _depend on_ them and must not duplicate, pre-empt, or relax them.
- **Detection and auto-retire of in-review conflicts.** Already designed and largely shipped in [`merge-conflict-handling-in-review.md`](merge-conflict-handling-in-review.md) (`conflict_watch`, `merge_poller`). We plug rungs _into_ that path; we do not re-design detection.
- **Stacked-PR mechanical rebase on base-merge.** Owned by [`auto-rebase-stacked-prs.md`](auto-rebase-stacked-prs.md). We reuse its engine-direct tier and its conflict-diagnosis collector; we do not re-implement them.
- **A general merge queue / serial-merge gate (bors/GitHub merge queue).** Explicitly rejected below — it serializes and does not reduce frequency.
- **Hard file locks or "one writer per file" dispatch.** Rejected — it is serialization by another name and violates the parallelism constraint.
- **Cross-repo / cross-product conflicts.** Same scope boundary the sibling designs already draw.
- **A learned/ML conflict predictor.** Predictions here are cheap deterministic signals (file overlap, speculative rebase), not a model.
- **Auto-merging PRs after resolution.** Resolution returns the PR to review; merge policy is unchanged.

---

## What already exists (compose, don't rebuild)

A deliberate inventory, because most of the substrate is already shipped and the design's cost hinges on reuse:

| Capability                                                                                    | Where                                                                                                                                                                                                 | Reuse in this design                                                                                                                   |
| --------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| **Conflict detection + auto-retire** for in-review PRs                                        | `merge_poller.rs` (`classify_state` `:1212`), `conflict_watch::on_conflict_detected` (`:105`)                                                                                                         | The insertion point for the escalation ladder.                                                                                         |
| **Conflict-resolution attempt records** (side table)                                          | `conflict_resolutions` table (`migrations_b.rs:215`), `insert_conflict_resolution` (`conflict_res.rs:66`); churn guard (4th attempt / 1h)                                                             | Extend as the telemetry substrate; add producer-side + per-rung rows.                                                                  |
| **Per-file conflict diagnosis**                                                               | `boss_conflict_diagnosis` crate: `ConflictDiagnosis { files: Vec<ConflictedFile> }`, `ConflictedFile { path, marker_count, shape }` via `git merge-tree`; collected pre-spawn (`coordinator.rs:4229`) | The raw hotspot data already exists per event — we aggregate it, we don't collect it anew.                                             |
| **Engine-direct mechanical rebase** (no worker)                                               | `auto_rebase` two-tier (`jj rebase`, ~5–15s, try-and-fall-back) + `cube workspace rebase` reporting `REBASED_CLEAN`/`REBASED_WITH_CONFLICTS` (`runner.rs:2311`)                                       | Rung 1, promoted onto the `conflict_watch` path.                                                                                       |
| **Pre-leased spawn with pre-loaded conflict**                                                 | `auto_rebase` escalation hands the worker an already-dirty workspace                                                                                                                                  | Rung 2 pre-staging.                                                                                                                    |
| **Counter metrics** (in-memory → `state.db`, 30s flush)                                       | `metrics/` framework; `CONFLICT_FLAGGED`/`CONFLICT_CLEARED` in `merge_poller.rs:106`                                                                                                                  | Per-class conflict-rate counters, scopable per-product (Layer 0).                                                                      |
| **Automations** (cron standing instruction → triage → files one task; dedicated 3-agent pool) | `Automation` (`types.rs:228`), `AutomationTrigger::Schedule{cron,tz}`, `create_automation` (`work/automations.rs:47`), `automation_scheduler.rs`                                                      | Direction 1's proactive refactoring chore is a _data insert_, not new infra.                                                           |
| **Task effort/model knobs**                                                                   | `CreateChoreInput.effort_level`, `.model_override`, `.driver`, `.depends_on`                                                                                                                          | Rung 2's small/cheap agent profile; refactoring chore sizing.                                                                          |
| **Dependency edges with a `relation` column**                                                 | `work_item_dependencies (dependent_id, prerequisite_id, relation DEFAULT 'blocks')`; **only `relation='blocks'` gates dispatch** (`dep_helpers.rs:191,203`)                                           | Direction 2's non-blocking `merge_order` relation slots in here.                                                                       |
| **Dispatch already prioritizes resolution**                                                   | `DispatchClass::MergeConflictRevision = 1` (`dispatch_class.rs`) — conflict fixes dispatch before all other work                                                                                      | The ladder inherits this priority; nothing to change.                                                                                  |
| **Per-PR-chain single-writer serialization**                                                  | `live_execution_elsewhere_in_chain` (`coordinator.rs:1762`)                                                                                                                                           | Precedent for scheduling, but keyed on PR-chain, _not_ file overlap — we deliberately do **not** extend it to hard-serialize siblings. |
| **The task expander (Planner)**                                                               | `planner.rs`; parallelism = _absence of edges_ (`:427`); "leave independently-startable tasks unedged"                                                                                                | Direction 2's declare-time file-overlap hint (P5-lite) goes in the prompt at `:427`.                                                   |
| **Project design-doc pointer**                                                                | `boss project set-design-doc <selector> --path`                                                                                                                                                       | Set after this PR.                                                                                                                     |

The gap this design fills is narrow and specific: **the `conflict_watch` path today goes straight to a full worker with no rung 0 or rung 1 attempt, the telemetry only sees in-review conflicts, and nothing sequences merges of overlapping siblings or refactors the hotspots that generate them.**

---

## Alternatives considered

Four coherent whole-program alternatives were considered and rejected; each fails the parallelism constraint, the generality constraint, or the speed goal.

### A. Global serialization / per-file locks

Give each file (or hot file) a lock; a task that wants to edit a locked file waits. Or, more crudely, cap dispatch so overlapping tasks never run concurrently.

**Rejected.** This is the operator's explicitly forbidden outcome — "smart enough to avoid completely serializing everything; we still want high parallelism." Hot files like `completion.rs`/`coordinator.rs` are touched by a large fraction of tasks; locking them would serialize most of the backlog behind them. It also does nothing for lockfiles (every task touches them) except serialize _everything_. It trades a solvable problem (fast resolution) for an unacceptable one (throughput collapse).

### B. A merge queue (bors / GitHub merge queue)

Land PRs through a serial queue that rebases-and-tests each PR against the queue head before merging.

**Rejected.** A merge queue serializes _merges_ (throughput hit at exactly the moment of highest contention), still requires a resolver when the rebase conflicts, does not reduce the _frequency_ of conflicts (it just relocates when they surface), ignores jj entirely, and does nothing for producer-side conflicts that happen before a PR is even queued. It is a heavyweight answer to a question we are not asking. The non-blocking `merge_order` relation (direction 2) captures the _useful_ half of a merge queue — ordering the risky pairs — without the serialization.

_Postmortem note (2026-07-19): six days after this rejection, flunge adopted the Trunk merge queue (P2951, [`trunk-merge-queue-integration`](trunk-merge-queue-integration-queue-backed-merges-merging-ui.md)) — for a failure class this rejection never considered (semantic merge races between individually-green PRs, with zero textual conflict). The rejection's reasoning stands for what it argued; see [the postmortem's reconciliation](#the-merge-queue-reversal-p2951-reconciled)._

### C. "Just make the resolution agent faster" (prompt-only)

Keep the single-worker path; make its prompt tighter and its model cheaper. No deterministic resolvers, no mechanical rung.

**Rejected as a _sole_ solution** (it survives as rung 2). Leaving the largest, most formulaic class — lockfiles, the #1/#2 conflict files — on _any_ agent path is strictly worse than a deterministic resolver: an agent regenerating `Cargo.lock` is slower, costs tokens, and can get it wrong, where a `cargo generate-lockfile` cannot. Prompt-tuning is necessary for the semantic residue but must sit _above_ rungs 0 and 1, not replace them.

### D. Big-bang repo restructuring

Split every hotspot file, convert every append-only structure to a keyed map, in one large upfront effort.

**Rejected.** High-risk, unbounded, and overfit to today's mono layout — the moment a new hotspot emerges the effort is stale. The telemetry-driven incremental automation (direction 1) gets the same structural benefit as a _standing_ process that continuously targets the current top offender, generalizes to any repo, and never requires a risky flag day. Precedent: `types.rs` was de-conflicted incrementally (the builder-pattern adoption), not in a big bang.

### Rejected sub-mechanism (worth recording)

**Extending `live_execution_elsewhere_in_chain` to hard-serialize file-overlap siblings.** Tempting because the hook exists, but a `blocks`-style gate on file overlap _is_ serialization — it would refuse to dispatch the second of any overlapping pair. Direction 2 instead uses a _non-blocking_ relation that lets both dispatch and only orders their merges. Recorded here so the coordinator sees it was considered and consciously rejected.

---

## Chosen approach: a layered program

Five layers, sequenced so each rests on the one before. Layer 0 first (you cannot improve what you cannot measure), then the ladder (the biggest, most certain win), then frequency reduction.

### Layer 0 — Telemetry (measurement first)

The substrate mostly exists; the work is _coverage_ and _aggregation_.

1. **Close the coverage gap.** Today only conflicts that reach `conflict_watch` (an in-review PR whose `main` moved) are recorded. Add recording for the two blind spots:
   - **Producer-side conflicts** — when a normal worker runs `cube workspace rebase` during its own task and gets `REBASED_WITH_CONFLICTS`, record a conflict event (files, shape) even though it never became an in-review bot comment. This is the bulk of the undercount.
   - **Per-rung outcomes** — every conflict event records which rung resolved it (0/1/2/3), how long, and whether it escalated. This is what tells us the ladder's payoff and where it leaks.
2. **Aggregate into a hotspot report.** A query surface over `conflict_resolutions.conflict_diagnosis` producing: per-file conflict frequency, per-file-_pair_ co-conflict frequency (which two files conflict together — the signal for direction 2), and per-class counts (lockfile / build / registry / migration / semantic, classified off `ConflictedFile.path` + `shape`). Expose as `boss engine conflicts hotspots` (implementation detail; the point is a machine-readable report the automation and the operator both consume). The report is **scoped per-product** (a required `--product` filter or per-product sections, never a cross-product blend): hotspot data is only meaningful within one repo, and boss manages several.
3. **Counters — must be scopable per-product.** Add per-class counters next to the existing `CONFLICT_FLAGGED`/`CONFLICT_CLEARED`, incremented at resolution time, so conflict rate and the rung mix are visible over time without a query. **Hard requirement: every metric or counter this design introduces must be scopable per-product** — a fleet-wide number that cannot be broken down by product hides which repo is hurting. The current counter framework is deliberately dimensionless (its design defers tags/dimensions as "a follow-up design"), so this requirement is satisfied one of two ways: (a) key the counters per-product (e.g. `conflict.<product>.<class>.resolved` as separate counters — product count is small and bounded, so cardinality stays sane), or (b) do the framework's anticipated follow-up and add a product dimension. Either is acceptable; what is not acceptable is shipping product-blind conflict metrics. The `conflict_resolutions` rows are already attributable to a product via their PR/task, so the query-backed report gets per-product scoping for free — this requirement bites only on the flat counters.

**Why first:** the reported ~12% is a floor we know is wrong; the whole cost/benefit of every later layer depends on the true rate and the class mix. It is also cheap — the per-file data is already collected per event.

### Layer 1 — The resolution escalation ladder

Restructure the `conflict_watch` resolution path from "detect → full worker" into "detect → rung 0 → rung 1 → rung 2 → rung 3," each rung gated on the previous declining — plus a direct escalation to rung 3 when a completed resolution is rejected by post-resolution review (see rung 3). Concretely, the engine-side harness (a small state machine invoked from `conflict_watch::on_conflict_detected`, before `maybe_spawn_conflict_revision`) does:

**Rung 0 — Deterministic resolvers (direction 5).** This rung is a **general, extensible engine feature, not a special case for any particular file.** The engine core knows nothing about lockfiles, bazel, cargo, or any other repo tooling choice; all format knowledge lives in individual resolvers behind a registry, each implementing roughly:

```
trait DeterministicResolver {
    fn class(&self) -> ConflictClass;             // for telemetry
    fn applies_to(&self, f: &ConflictedFile) -> bool;
    fn resolve(&self, ws: &Workspace, f: &ConflictedFile) -> ResolveOutcome; // Resolved | Declined | Failed
}
```

The registry is the extension seam: today boss ships a set of **built-in** resolvers for particular file types; in the future, users of boss can plug in their own (see the declarative-recipe mechanism below). Deterministic resolvers are intended to be the _mainstay_ of how this common conflict surface is handled — the design bet is that most conflicts by volume are formulaic, and formula beats agent on speed, cost, and reliability.

The first built-in is **lockfiles**, because the data says it is the single highest-value target:

- `Cargo.lock`: discard the conflicted file, run the lockfile-regeneration command against the merged `Cargo.toml` (`cargo generate-lockfile` / equivalent), so both sides' dependency edits are represented.
- `MODULE.bazel.lock`: the bazel analogue (regenerate the lockfile from the merged `MODULE.bazel`). Note that `bazel mod deps --lockfile_mode=update` re-evaluates every module extension, including `rules_swift_package_manager`'s `swift_deps`, which runs `swift package describe` against `tools/boss/app-macos/Package.swift` and therefore needs the gitignored `ThirdParty/GhosttyKit.xcframework`. A freshly provisioned cold cube workspace never has it, so mono's `.cube/setup.yaml` materializes a parse-only stub (`tools/boss/app-macos/scripts/stub-ghosttykit-xcframework.sh`) on lease; without it this resolver fails environmentally on every cold workspace.
  The harness runs `cube workspace rebase`; for each `REBASED_WITH_CONFLICTS` file it asks the registry. **If, and only if, _every_ conflicted file is resolved by some resolver**, it commits, pushes, comments, and auto-retires — no agent, zero tokens. If any file is declined _or fails_, it discards rung-0 partial work and climbs. `Declined` and `Failed` are kept distinct so the ladder verdict tells the truth: `Declined` means "no resolver / not a formulaic conflict" (benign), whereas `Failed` means a resolver matched and ran but its command errored operationally (e.g. a broken/incomplete workspace environment) — an actionable, fixable condition rather than "rung 0 doesn't apply here". Deterministic resolvers are structurally _preserving_ (they represent both sides), so they pass the T2253 tripwire by construction — but the tripwire still runs on the result (see composition below). Later built-ins (behind telemetry): reformat-only conflicts (`rustfmt` reflow), regenerable generated code, and pure-append registry unions where both sides only added distinct entries. **New resolvers are authored by agents** — direction 1's automation files "write a resolver for class X" tasks as telemetry surfaces new formulaic classes. That is the direction-1↔direction-5 loop the operator called out.

**Extension mechanism — declarative resolution recipes.** Beyond code resolvers compiled into boss, the natural extension point is a **declarative recipe**: a mapping from a file pattern to a resolution formula, e.g. "for `*.lock`: discard the conflicted file, run `<command>`, verify with `<command>`." Two candidate homes, not mutually exclusive:

- **In the target repo** (e.g. a `.boss/conflict-recipes.toml`): the repo declares how its own generated/formulaic files regenerate. Best locality — the knowledge lives next to the tooling it describes, travels with the repo, and repo owners extend it without touching boss.
- **In boss product config**: the operator declares recipes per product. Safer trust model — recipes execute commands in the workspace, and boss-side config means a PR to the target repo cannot alter what the engine will execute on the next conflict (a repo-side recipe file is attacker-adjacent input in a world of agent-authored PRs).

The recommendation is to design the recipe format alongside T2's trait (a recipe is just a data-driven `DeterministicResolver` implementation — one generic "recipe resolver" interprets all recipes), ship boss-side product config first (trust-simple), and treat in-repo recipe files as a follow-up that must answer the trust question explicitly (e.g. only honor recipes already present on `main`, never from the PR branch). Recipes are deliberately **not** a v1 blocker — the built-in resolver set covers the measured hotspots — but the trait and telemetry are shaped so recipes drop in without rework (tracked as T13).

**Rung 1 — Engine-direct structural rebase (directions 3 & 4).** This is `auto_rebase`'s existing engine-direct tier, promoted onto the `conflict_watch` path. Run `cube workspace rebase` engine-side (not via an agent). Because jj represents conflicts first-class and does a real 3-way structural merge, two things happen for free: (a) if GitHub's `CONFLICTING` was stale or the overlap was only apparent, it returns `REBASED_CLEAN` → done, no agent; (b) non-overlapping hunks in the "same file" auto-resolve, leaving only genuinely overlapping hunks — this is the operator's "auto-resolve trivially non-overlapping conflicts" for free. Whatever files remain conflicted after rung 1 are the residue that climbs to rung 2. (Rungs 0 and 1 interleave in practice: rung 0 runs against the post-rung-1 residue; the harness attempts the mechanical rebase, then hands the still-conflicted files to the resolver registry.)

**Rung 2 — Small focused, pre-staged resolution agent (direction 3).** For genuine semantic overlap on a bounded set of files, spawn a resolution agent that is _not_ a cold full worker:

- **Pre-staged.** The engine has already leased the workspace, run `cube workspace goto --pr` + `cube workspace rebase`, and left it dirty at the conflict. The agent starts _at_ the conflict — the fetch/checkout/rebase latency is off its critical path (today those are prompt-driven worker steps, `runner.rs:2310`). This is the "pre-staged workspace so the agent starts at the conflict rather than at clone/fetch" idea, made real.
- **Small + cheap.** `effort_level = small`, a `model_override` to a fast model; a tight prompt that says "resolve _only_ these conflicted hunks; the diagnosis is inline; do not author new work; the preservation rule (T2253 P1) applies." Bounded scope, bounded model, bounded prompt.
- It resolves the hunks and returns; it does not re-plan the PR.

**Rung 3 — Full worker (unchanged fallback).** Reached two ways:

- **Decline** — rung 2 declines the conflict up front (large/architectural conflict, or a stop condition like `product_decision_required`).
- **Escalation on review rejection** — a lower rung _completed_ a resolution, but post-resolution review rejected it: the T2253 deletion tripwire fired, the build gate failed, or an AI review agent examining the resolution judged it wrong. In that case the conflict escalates to rung 3 **with the review findings attached to the brief** — it does not retry the same rung, and it does not loop. This is the safety net for making rung 2 faster and simpler: we have prior incidents of resolution agents doing destructive things to make a conflict go away (incident-002's agent deleted merged functionality, and review blessed it), and a cheaper/faster rung-2 agent _raises_ that risk. Escalation-on-rejection bounds it — a bad cheap resolution costs one review round and then gets the full-strength path, never a merge.

Rung 3 itself is identical to today's path except it inherits the pre-staged workspace, the review findings when escalated, and — once T2253 lands — the hardened preservation brief.

**Escalation is telemetry-visible:** each rung records its outcome (Layer 0), so we can see, e.g., "62% of conflicts resolve at rung 0/1 with no agent" and target the rest.

### Layer 2 — Proactive hotspot-refactoring automation (direction 1)

Reduce _structural_ frequency by continuously de-hotspotting the repo, using the shipped Automations mechanism — this is a data insert, not new infrastructure.

Create a standing `Automation` (cron trigger, e.g. weekly, running in the dedicated 3-agent automations pool so it never contends with interactive work) whose `standing_instruction` is roughly:

> "Read the conflict-hotspot report (`boss engine conflicts hotspots`). If a file or pattern is a repeat offender **and** has a known de-conflicting refactor, file **exactly one** task to do it. Known refactors, in priority order: (a) if a formulaic conflict class is unhandled, author a deterministic resolver for it (Layer 1 rung 0); (b) split an oversized hotspot file into cohesive smaller units; (c) convert an append-only structure (registry, big enum, match arm list) into a keyed map or a per-entry file; (d) adopt the builder pattern for a struct that additive PRs keep touching (as `types.rs` already did). Prefer the highest-frequency hotspot the last few chores did not already address. If nothing clears the bar, SKIP."

The automation's triage agent emits one task (or SKIP); the task lands in a worker pane, produces a PR, and goes through review like anything else. The `open_task_limit` caps in-flight refactors so the automation never floods the backlog. This closes the loop **telemetry → automation → refactor → fewer conflicts**, and — critically — it _generalizes_: the report is repo-agnostic, and the refactor catalog is generic patterns, not mono specifics.

### Layer 3 — Conflict-aware scheduling (direction 2), without serializing

Reduce _temporal_ frequency by sequencing the merges of likely-overlapping siblings — while both still dispatch in parallel. Two composed sub-parts, matching the operator's "expander hint vs. scheduler enforcement":

1. **Declare-time hint (the P5-lite half, promoted from T2253).** Extend the Planner prompt (`planner.rs:427`, the "maximise safe parallelism" section) so that when it declares two tasks parallel it _also_ considers likely file overlap, and for a high-overlap sibling pair emits a **soft merge-order annotation** instead of a hard `blocks` edge. This is deliberately lightweight — a prompt-level nudge, exactly the P5-lite intent — and it is the _only_ change if the fuller mechanism is deferred.
2. **Enforcement (the fuller half).** A new dependency relation **`merge_order`** in `work_item_dependencies`. Because dispatch gating keys strictly on `relation='blocks'` (`dep_helpers.rs:191`), a `merge_order` edge **does not gate dispatch** — both siblings start immediately, full parallelism preserved. What it does:
   - **Orders the merge.** When both siblings are ready to merge, the `merge_order` edge names which lands first; the later one's forward-port is stamped with a preservation contract (feeding the T2253 tripwire) so it reconciles rather than clobbers. This is incident-002's Layer-D "merge-order contract" as a _soft_ edge.
   - **Optional bounded stagger (tunable, off by default).** For only the _highest_-overlap pairs, optionally delay the _second_ task's dispatch by a small capped window (minutes, not "until the first merges") so their diffs interleave less. This is a knob, hard-capped, applied to a tiny minority of pairs; it is _not_ a block and never waits for a merge. Ships behind a config flag, defaulted conservative.

**Why this preserves parallelism:** `merge_order` is non-blocking by construction; a `blocks` edge remains reserved for true prerequisites (the Planner's existing rule is unchanged). File overlap alone never produces a `blocks` edge. The existing per-PR-chain serialization (`live_execution_elsewhere_in_chain`) is _not_ extended to siblings. The result: overlapping siblings run concurrently and only their _merge order_ (and the later one's preservation obligation) is constrained — which is where the actual damage (incident-002) happened, not at dispatch.

**Composition with T2253's P5-lite:** they are the same idea at two altitudes. P5-lite is the expander _hint_; direction 2's `merge_order` relation is the _enforcement_. If T2253 ships P5-lite first, direction 2 consumes its annotation as the `merge_order` edge source; if this project ships first, it _is_ P5-lite plus the enforcement. Either way there is exactly one file-overlap signal and one soft edge — **no double-serialization.**

### Layer 4 (phase 2) — jj-native speculative conflict prediction (direction 4)

Convert "conflict discovered at review time" into "conflict predicted while both branches are in flight." Piggybacking on the `merge_poller` sweep, periodically run an **engine-direct speculative rebase** of each in-flight PR branch onto current `main` (a throwaway `cube workspace rebase` on a scratch, no worker, no push). Outcomes:

- **Clean** → nothing to do; record the negative.
- **Conflict predicted** → record it to telemetry _early_ (improves the hotspot signal before a PR is even in review), and optionally trigger rung 0/1 pre-emptively so the fix is ready the instant the base actually moves.
- **Two in-flight branches predicted to conflict** → offer to convert them into an ordered **stack** (the would-be conflict becomes an ordered stack handled by the existing `auto_rebase` machinery) — the operator's "stacked-PR structures that convert would-be conflicts into ordered stacks."

This is marked **phase 2 / not a v1 blocker**: it is high-value but depends on the ladder (rungs 0/1 as the pre-emptive resolvers) and telemetry being mature, and speculative rebases must be rate-limited to avoid churn. It is designed here so the coordinator sees the full arc, not deferred silently.

### Composition with T2253 (the safety gates)

This is the load-bearing constraint, stated explicitly because "faster/lighter resolution must still pass the deletion tripwire":

- **The gates run on the _result_, not the rung.** T2253's P2 both-parents deletion tripwire diffs the resolution against _both_ merge parents and halts on any net removal of merged functionality. That check is indifferent to whether rung 0, 1, 2, or 3 produced the diff. So every rung — including the free deterministic ones — routes its output through the _same_ tripwire and the _same_ build gate before auto-retiring. Speed changes _who resolves_; it does not change _what is verified_.
- **Deterministic resolvers are preserving by construction**, so they should pass the tripwire cleanly — but we do **not** skip the tripwire for them (that would be a bypass). If a deterministic resolver ever produces a net deletion, that is a resolver bug the tripwire _should_ catch.
- **A gate or review rejection escalates; it never retries the same rung.** When any rung's completed resolution is rejected — tripwire, build gate, or AI reviewer — the conflict climbs to rung 3 with the findings attached (see rung 3). The cheap rungs get exactly one shot; the safety net is escalation, not iteration on the cheap path.
- **Rung 2's small agent inherits the preservation brief.** Once T2253's P1 preservation clause exists, the small-agent prompt includes it verbatim; the small agent is _more_ constrained than today's worker (bounded to the conflicted hunks), not less.
- **Ordering dependency.** The safety-critical integration point (routing every rung through the tripwire, adding the preservation clause to the small-agent prompt) has an explicit external dependency on **T2253's P2/P1 landing**. It is called out as such in the task breakdown. Until T2253 lands, rungs 0/1 still run (they are preserving) but rung 2's prompt uses today's brief and the result still passes today's checks.

### Composition with `auto_rebase` / `conflict_watch` precedence

The in-review conflict design already defines a cross-flow precedence `rebase > conflict > ci` and a unified opt-out `products.auto_pr_maintenance_enabled`. The ladder lives _inside_ the `conflict` slot of that ordering — it does not add a fourth concern, it makes the `conflict` handler smarter. The opt-out flag continues to disable the whole thing per product. Rung 1 literally _is_ `auto_rebase`'s engine-direct tier reused, so the two designs converge on one mechanical-rebase implementation rather than two.

### Phased rollout (summary)

1. **Phase 1 (measurement):** Layer 0 — telemetry coverage + hotspot report + counters.
2. **Phase 2 (the big win):** Layer 1 rungs 0 (resolver framework + built-in lockfile resolvers) and 1 (engine-direct rebase) on the `conflict_watch` path. This alone should take the ~8%+ mechanical fraction to zero-agent, instantly.
3. **Phase 3:** Layer 1 rung 2 (pre-staged small agent) + the T2253 integration.
4. **Phase 4:** Layer 2 (refactoring automation) — starts paying down structural frequency once the hotspot report exists.
5. **Phase 5:** Layer 3 (conflict-aware scheduling), expander hint first, then the `merge_order` relation.
6. **Phase 6 (future):** Layer 4 (speculative prediction, stacked-PR structuring).

---

## Risks / open questions

- **Deterministic lockfile regeneration runs a build tool.** `cargo generate-lockfile` / the bazel lockfile update run a real command with a real dependency graph. Risks: it is slower than a text merge (still far faster and cheaper than an agent), it can _itself_ fail if the merged manifest is inconsistent (then rung 0 declines and we climb — safe), and it must run in the leased workspace with the right toolchain. Open question: do we auto-apply a regenerated lockfile with no agent/human in the loop, trusting the build gate to catch a bad regen, or gate the first N auto-regens behind a light review? (See attentions.)
- **Declarative recipes execute commands.** A resolution recipe (rung 0's extension mechanism) is ultimately "run this command in the workspace when this file conflicts." Boss-side product config is the trust-simple home (only the operator writes it). An in-repo recipe file is more convenient but is attacker-adjacent input in a world of agent-authored PRs — a PR could rewrite the recipe the engine will execute. Open question: if/when we honor in-repo recipes, is "only read the recipe file as it exists on `main`, never from the PR branch" a sufficient trust boundary, or do recipes need operator approval on change? (See T13; boss-config-first sidesteps this for v1.)
- **The `REBASED_CLEAN`-after-`CONFLICTING` case.** GitHub says `CONFLICTING` but jj rebases clean surprisingly often (different merge algorithms). Rung 1 turns those into instant no-agent retires — but we should confirm the frequency (telemetry will) so we size the win correctly.
- **Rung 2 model choice.** Which fast model for the small resolution agent, and do we trust it on multi-file semantic conflicts or cap it to single-file? A too-cheap model that resolves badly gets caught by post-resolution review and escalates to rung 3 with the findings (escalation-on-rejection) — one wasted review round, but never a bad merge. Open question in attentions.
- **Direction 2 overlap prediction is fuzzy.** The expander's file-overlap guess is a prompt-level heuristic; it will have false positives (unnecessary `merge_order` edges — harmless, they don't block) and false negatives (missed pairs — no worse than today). The `merge_order` edge is cheap enough that erring toward more edges is fine. The _stagger_ knob is the risky part (it does delay dispatch), which is why it defaults off and is hard-capped.
- **Automation churn.** A refactoring automation that files too aggressively could itself generate conflicts (a hotspot-split PR is a big diff). `open_task_limit = 1` and a "don't re-target what recent chores addressed" instruction bound this; still, the automation's own PRs want priority/scheduling care so they land before they rot.
- **Sequencing against T2253.** The safety integration (rung outputs → tripwire, preservation clause in the small-agent prompt) depends on T2253 landing. If this project moves faster than T2253, we ship rungs 0/1 (preserving, safe under today's checks) and hold rung 2's prompt hardening until P1 exists. Do reviewers want a hard gate on T2253 before _any_ ladder work, or is the rung-0/1-first sequencing acceptable? (See attentions.)
- **Generality claim.** The design is repo-agnostic except the individual resolvers. Worth a reviewer sanity-check that nothing in the telemetry/automation/scheduling layers has quietly assumed the mono layout.

---

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Dependencies reference task names; "none" means it can start immediately. Tasks at the same depth with disjoint dependencies may run in parallel (noted per depth). Items marked **`future / not a v1 blocker`** are designed above but deliberately deferred.

### Depth 0 — may all run in parallel (no dependencies)

**T1. Conflict telemetry: coverage + schema**
Scope: Extend conflict recording so it captures the two blind spots — producer-side conflicts (a normal worker's `cube workspace rebase` returning `REBASED_WITH_CONFLICTS`) and a per-event `conflict_class` + `resolved_by_rung` field. Reuse the `conflict_resolutions` table / `ConflictDiagnosis` substrate; add columns, not a new table. Every record must be attributable to a product (via its PR/task) so all downstream aggregation and counters can scope per-product. No aggregation yet.
Effort hint: `medium`.
Dependencies: none.

**T2. Deterministic resolver framework + built-in lockfile resolvers (rung 0)**
Scope: Introduce the `DeterministicResolver` registry (trait, class enum, dispatch by `ConflictedFile`) as its own crate (per the crate-per-unit convention). The framework is the deliverable: engine core stays agnostic to repo tooling, all format knowledge lives in registered resolvers, and the registry is the future user-extension seam (T13's recipes become one generic resolver behind it). Ship the built-in lockfile resolvers (`Cargo.lock` + `MODULE.bazel.lock` regenerate-from-merged-manifest) as the first instances. Unit-tested standalone against fixture conflicts; not yet wired into `conflict_watch` (that is T4).
Effort hint: `medium`.
Dependencies: none.

**T3. Expander file-overlap hint (P5-lite)**
Scope: Extend the Planner prompt (`planner.rs:427`) to consider likely file overlap when declaring tasks parallel and to emit a soft merge-order annotation (not a `blocks` edge) for high-overlap sibling pairs. Prompt + `emit_task_graph` validation only; consumes nothing new. This is the lightweight P5-lite; it stands alone even if T8 is deferred. Reconcile with T2253 so there is one hint, not two.
Effort hint: `small`.
Dependencies: none (coordinate with T2253's P5-lite so they are the same change).

### Depth 1

**T4. Escalation-ladder harness: rungs 0 + 1 on the `conflict_watch` path**
Scope: Insert the ladder state machine into `conflict_watch::on_conflict_detected` _before_ `maybe_spawn_conflict_revision`. Run engine-direct `cube workspace rebase` (rung 1); on `REBASED_CLEAN` auto-retire with no agent; feed residual conflicted files to the T2 resolver registry (rung 0); if all resolved, commit/push/comment/auto-retire; else fall through to the existing worker spawn. The harness owns escalation-on-rejection: when a rung's completed resolution is rejected post-resolution (build gate now; tripwire/AI review once T9 lands), escalate to rung 3 with the findings attached — never retry the same rung. Reuse `auto_rebase`'s engine-direct tier and the conflict-diagnosis collector. Record rung outcomes via T1.
Effort hint: `large`.
Dependencies: T1, T2.

**T5. Hotspot aggregation report + counters**
Scope: Build the query surface over `conflict_resolutions.conflict_diagnosis` — per-file frequency, per-file-pair co-conflict frequency, per-class counts — exposed as `boss engine conflicts hotspots` (machine-readable, scoped per-product). Add per-class conflict counters alongside `CONFLICT_FLAGGED`; per the Layer 0 hard requirement they must be scopable per-product (per-product-keyed counters, or the counter framework's deferred dimension follow-up). Consumes the richer records from T1 (and, once T4 lands, the rung mix).
Effort hint: `small`.
Dependencies: T1.

_Depth-1 parallelism: T4 and T5 are independent (both depend only on T1/T2) and may run in parallel._

### Depth 2

**T6. Rung 2: pre-staged small focused resolution agent**
Scope: Engine pre-stages the workspace (lease + `cube workspace goto --pr` + `cube workspace rebase`) so the agent starts dirty at the conflict; define the small-agent profile (`effort_level=small`, `model_override` to a fast model, tight conflict-only prompt with the diagnosis inline). Wire as rung 2 between rung 0/1 and the full-worker fallback in the T4 harness.
Effort hint: `medium`.
Dependencies: T4.

**T7. Proactive hotspot-refactoring Automation (direction 1)**
Scope: Create the standing `Automation` (cron, automations pool) whose `standing_instruction` consumes the T5 hotspot report and files exactly one refactoring task (or a "author a resolver for class X" task, or SKIP). Pure data insert via `create_automation` + the standing-instruction text; `open_task_limit` caps in-flight refactors.
Effort hint: `small`.
Dependencies: T5.

_Depth-2 parallelism: T6 and T7 are independent and may run in parallel._

### Depth 3

**T8. Non-blocking `merge_order` relation + merge sequencing (direction 2 enforcement)**
Scope: Add the `merge_order` relation to `work_item_dependencies` (must NOT gate dispatch — confirm the `relation='blocks'`-only gating stays intact). The engine consumes T3's annotation to create `merge_order` edges; at merge time it orders the pair and stamps the later PR's forward-port with a preservation contract feeding the tripwire. Include the bounded dispatch-stagger knob (config-flagged, default off, hard-capped) for the highest-overlap pairs only.
Effort hint: `large`.
Dependencies: T3, T1 (overlap/telemetry signal).

**T9. T2253 safety integration for the ladder**
Scope: Ensure every rung's output routes through T2253's P2 both-parents deletion tripwire and the build gate before auto-retiring, and add T2253's P1 preservation clause to the rung-2 small-agent prompt. Wire tripwire/AI-review rejections into T4's escalation-on-rejection path so a rejected resolution escalates to rung 3 with the review findings attached. Thin integration + tests asserting a deletion produced by _any_ rung is halted identically and escalates rather than retries. Explicit **external dependency on T2253 (P1/P2) landing**; until then, rungs 0/1 run under today's checks and this task is blocked.
Effort hint: `medium`.
Dependencies: T6; **external: T2253 (P1/P2)**.

_Depth-3 parallelism: T8 and T9 are independent and may run in parallel (T9 additionally gated on the external T2253)._

### Future / not a v1 blocker

**T10. jj speculative conflict prediction (Layer 4)** — `future / not a v1 blocker`.
Scope: Piggyback on `merge_poller` to speculatively engine-direct-rebase in-flight PR branches onto `main`, record predicted conflicts to telemetry early, and optionally pre-run rung 0/1. Rate-limited to avoid churn.
Effort hint: `medium`.
Dependencies: T4, T5 (needs the ladder + telemetry mature).

**T11. Stacked-PR auto-structuring for predicted conflicts (Layer 4)** — `future / not a v1 blocker`.
Scope: When two in-flight branches are predicted to conflict, offer to convert them into an ordered stack handled by the existing `auto_rebase` machinery.
Effort hint: `large`.
Dependencies: T10, `auto-rebase-stacked-prs` shipped.

**T12. Additional built-in deterministic resolvers (Layer 1 rung 0, ongoing)** — `future / not a v1 blocker`.
Scope: As telemetry (T5) surfaces new formulaic classes, author resolvers for reformat-only conflicts, regenerable generated code, and pure-append registry unions. Each is a small task; these are the tasks the T7 automation files over time (the direction-1↔direction-5 loop).
Effort hint: `small` (each).
Dependencies: T5 (to identify the class), T2 (the framework).

**T13. Declarative resolution recipes (rung 0 extension mechanism)** — `future / not a v1 blocker`.
Scope: Design and implement the declarative recipe format (file-pattern → resolution formula) as one generic recipe-interpreting `DeterministicResolver` behind the T2 registry. Boss-side product config first; in-repo recipe files only after the trust question (recipes execute commands; agent-authored PRs could rewrite a repo-side file) is answered explicitly — e.g. only honoring recipes as they exist on `main`. This is what makes rung 0 user-extensible without a boss code change.
Effort hint: `medium`.
Dependencies: T2 (the framework), T4 (the harness that invokes it).

### Dependency graph at a glance

```
Depth 0:  T1        T2        T3
            \       /          |
Depth 1:     T4 ── (T2)       T5 ──┐        (T4, T5 parallel)
              |                |    |
Depth 2:     T6               T7   |        (T6, T7 parallel)
              |                     |
Depth 3:     T9*  ┌──────────────── T8      (T8, T9 parallel; T9 also needs T2253)
                  (T8 needs T3+T1)
Future:   T10 (needs T4,T5) → T11 ;  T12 (needs T2,T5) ;  T13 (needs T2,T4)
```

`*` T9 additionally gated on external **T2253 (P1/P2)**.

The critical path to the biggest, most certain win — lockfiles and mechanical conflicts becoming zero-agent — is **T1 + T2 → T4** (measurement + deterministic resolver + ladder harness). Everything else layers on top.

---

## Postmortem (2026-07-19)

- **Date:** 2026-07-19, six days after the design landed.
- **Provenance:** execution `exec_18c3e7eb568e5a48_e9b` (investigation), per the design-postmortem mechanism (T2935).
- **Scope:** as-built reality vs. this design — the project's own 15 tasks, **plus the fifteen fix rows and their satellites filed outside the project** between 2026-07-14 and 2026-07-19. The follow-up family is the core of this postmortem: the build took about 22 hours; making it production-real took six days.
- **Sources:** the merged PR record (bodies and diffs of mono #1957–#2139 and the flunge PRs cited below), the engine source as of this writing, and operator-reported engine telemetry where noted. Claims are attributed throughout.

### TL;DR

The design was implemented fast and faithfully: all 15 tasks landed as 14 PRs plus one data insert within ~22 hours of the design merging (2026-07-13 20:50Z → 2026-07-14 19:01Z). Structurally, what shipped is what was designed — the rung 0–3 ladder, the resolver-registry crate, the telemetry columns, the hotspot report, `merge_order` edges, speculative prediction, recipes. What the next six days revealed is that the design specified **rungs and gates, but not a lifecycle or an environment**: rung 0 went to production gated off with no alarm and scored zero lifetime live successes through 07-16 under three successive causes (a compile-time gate, a path-parsing mismatch, a non-hermetic workspace environment); transient infrastructure errors were conflated with genuine declines and bought full LLM revisions; an engine restart vaporized an in-flight attempt, leaked its workspace lease for cube's full 30-minute TTL, and left the conflict stranded; and two worker "success" claims had no verifier behind them. Every one of these defects degraded to either the expensive-but-correct rung-3 path or a stuck-but-safe wedge — **no ladder defect ever produced a wrong merge** — and each was root-caused and fixed within one to two days of surfacing. Separately, six days after this design rejected merge queues, flunge adopted the Trunk merge queue (P2951) — for a failure class this design's rejection never considered. The positions are compatible and now compose; see [the reconciliation](#the-merge-queue-reversal-p2951-reconciled).

### Timeline at a glance

| Date (2026, UTC) | What happened                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 07-13            | Design merged (#1791, 20:50Z). First three implementation PRs merge ~90 minutes later (#1961, #1957, #1958).                                                                                                                                                                                                                                                                                                                                                  |
| 07-14            | The remaining eleven project PRs merge by 19:01Z (#2001). Hardening starts the same day: the re-arm-loop wedge fix (#2009), eager revision executions (#2011), dead-attempt supersede (#2013). Near-duplicate test chores #1995/#1996 land 31 minutes apart.                                                                                                                                                                                                  |
| 07-15            | Rung-0 apply flipped live (#2043), triggered by the #2032 incident — a lockfile-only re-conflict spawned a second full LLM revision with no trace line explaining why. `cube pr update` detached-push guard (#2057).                                                                                                                                                                                                                                          |
| 07-16            | Engine verifies CI before honoring mark-succeeded-via-rebase (#2059). Era-B descriptor-suffix fix (#2080). Era-C1 lease fixes: cube quarantine (#2091), engine retry + revision-row close (#2093).                                                                                                                                                                                                                                                            |
| 07-17            | Rung-1 residue enumeration + decline-log fidelity (#2095). Era-C2 cold-workspace fix (#2098). conflict_watch test-file split (#2106).                                                                                                                                                                                                                                                                                                                         |
| 07-18            | **Live incident:** an engine restart (shutdown 11:08:56.943Z, new boot 11:08:58.047Z) kills conflict-ladder attempt `crz_18c35db426d18878_7d9` mid-rung-0 on flunge #960's conflict. The attempt vanishes with no verdict; its cube lease on `flunge-agent-035` sits orphaned until cube's 30-minute TTL sweep; nothing re-attempts the conflict.                                                                                                             |
| 07-19            | Incident remediation trio (#2136 attempt durability, #2135 post-restart re-detection, #2134 lease registry/reap/heartbeat). checkleft Node preflight (#2132); the stale system Node v20.7.0 is replaced operationally with v24.8.0. flunge #960 merges after its conflict is resolved by a manually filed worker revision (operator's account; the PR record is consistent with it). P2951 — Trunk merge queue for flunge — opens; its design lands as #2150. |

### What shipped (project proper)

All 15 tasks completed, mapped to this doc's task breakdown:

| Design task                                                         | Row   | PR                 | Merged | Dark at landing?                                                                                      |
| ------------------------------------------------------------------- | ----- | ------------------ | ------ | ----------------------------------------------------------------------------------------------------- |
| Design doc                                                          | T2265 | #1791              | 07-13  | —                                                                                                     |
| T1 telemetry coverage + schema                                      | T2554 | #1961              | 07-13  | Inert by design: every resolution stamped rung 3 until the ladder existed                             |
| T2 resolver framework + lockfile resolvers                          | T2555 | #1957              | 07-13  | Yes — "not yet wired into `conflict_watch`"                                                           |
| T3 expander overlap hint                                            | T2556 | #1958              | 07-13  | Hints emitted; nothing consumed them until #1969                                                      |
| T4 ladder harness, rung 1 (rung-0 apply split out to T2576)         | T2557 | #1968              | 07-14  | Yes — behind the default-off `conflict_ladder_mechanical_rebase` flag; rung-0 apply deferred to #1983 |
| T5 hotspot report + counters                                        | T2558 | #1967              | 07-14  | No                                                                                                    |
| T6 rung-2 small agent                                               | T2559 | #1979              | 07-14  | Reachable only through the flag-gated ladder                                                          |
| T7 hotspot-refactoring automation                                   | T2560 | data insert, no PR | 07-14  | —                                                                                                     |
| T8 `merge_order` sequencing                                         | T2561 | #1969              | 07-14  | Stagger knob default 0 (off)                                                                          |
| T9 T2253 safety integration                                         | T2562 | #2001              | 07-14  | Tripwire wired; deliberately did not flip rung 0 live                                                 |
| T10 speculative prediction                                          | T2563 | #1978              | 07-14  | Yes — default-off flag                                                                                |
| T11 stacked-PR auto-structuring                                     | T2564 | #1987              | 07-14  | Yes — default-off flag; advisory offers only                                                          |
| T12 additional resolvers (registry append-union)                    | T2565 | #1974              | 07-14  | Yes — until registered into `with_builtins()`                                                         |
| T13 declarative recipes                                             | T2566 | #1975              | 07-14  | Yes — **still unwired today** (see current state)                                                     |
| Rung-0 apply path (added mid-flight; not in the original breakdown) | T2576 | #1983              | 07-14  | Yes — `RUNG0_APPLY_LIVE = false` compile-time constant                                                |

Two structural notes. First, **T2576 was added mid-flight**: the design folded rung-0's commit+push into T4, but there was "no clean cube verb" for an engine-side push, so it became its own task delivering `cube workspace push` (compare-and-swap on the tracked bookmark, checkleft push gate included) plus `attempt_rung0`. Second, the two tasks this doc marked **"future / not a v1 blocker" (T10, T11) shipped inside the same 22-hour window anyway** — dark, behind default-off flags, with their expensive halves (pre-emptive rung runs; accept-and-convert, which depends on auto-rebase machinery that itself has not shipped) deferred. The "future" label did not survive contact with an eager task expander. Harmless here, but worth noticing: scope labels in a design are advisory to an automation that optimizes for completing rows.

### Where each goal actually stands

- **"Measure the real conflict rate and surface."** The substrate shipped: schema v25 columns (`event_source`, `conflict_class`, `resolved_by_rung`), the producer-side `boss engine conflicts record-producer` CLI, `boss engine conflicts hotspots --product` (always product-scoped, per the hard requirement), and per-product per-class dynamic counters. But the goal's spirit — _we know what the system is doing_ — was not met at rollout: the ladder's own decision points were uninstrumented (theme 3), and the design's flagship question ("what is the true rate vs. the ~12% floor?") still has no recorded answer. Producer-side recording also rests on workers remembering a CLI call — a prose-contract seam of exactly the kind the worker-proposal-API design (2026-07-19) now exists to replace.
- **"Make the common conflict free" (rung 0).** Shipped, and live today — but it took until 07-15 to be turned on and until 07-17 to become _capable_ of a live success (three successive zero-success causes, below). Operator-reported lifetime telemetry stood at **0 of 42 live attempts through 07-16** (engine telemetry, not the PR record; #2098's body independently states the bazel resolver had "a 0% live success rate"). As of this postmortem the PR record contains no confirmed live rung-0 success — the first post-fix live attempt on record (07-18) was the one killed mid-rung by the engine restart.
- **"Make the mechanical conflict agent-free" (rung 1).** Shipped and operating behind the deliberately default-off, operator-enabled flag. Its reporting needed two fidelity fixes (#2095): residue enumeration saw only working-copy-tip conflicts while `jj git push` refuses the whole range for any conflicted ancestor commit, and declines were logged under the wrong label.
- **"Make the semantic conflict cheap and fast" (rung 2).** Half-met. Shipped with the conservative single-file cap (per the attentions question), on Sonnet at `effort_level = trivial` (which floors to Sonnet, never Haiku; `small` was rejected because it dispatched byte-identical knobs to rung 3 — the design's "cheap model" is in practice "same model, lower effort"). But the design's headline latency win — the pre-staged workspace, "the agent starts AT the conflict" — was deferred (#1979 `[deferred-scope]`): the coordinator's "positioning is never skipped for revisions" invariant means rung 2 still pays a second lease+goto+rebase.
- **"Reduce frequency without reducing parallelism."** The mechanisms shipped: hints (#1958) materialize into non-blocking `merge_order` edges (#1969, dispatch gating confirmed `blocks`-only, stagger knob `BOSS_MERGE_ORDER_STAGGER_SECS` default 0, clamped at 600s), and the T2560 automation stands. There is no evidence yet that frequency moved; the window was consumed by hardening. The automation's first observable output is ironic: near-duplicate test chores #1995/#1996 filed against the same function 31 minutes apart — the duplication is now the tracked automation-dedup defect (T2944), and the duplicate pair produced its own merge conflict.
- **"Generalize."** Held for the engine core — the registry/trait/recipe shapes are repo-agnostic, and the extension seam worked (the append-union resolver and the recipe resolver both dropped in with no registry API change). Undermined in practice by the _resolvers'_ environmental assumptions — theme 2.
- **"Compose with T2253, never weaken it."** Met, and in one place deliberately exceeded: instead of the design's escalation-on-rejection for tripwire hits, #2001 wired them to **halt for operator sign-off** (`blocked:deletion_signoff`) — on the argument that dispatching an agent to "fix" a flagged deletion is incident-002's failure mode, not a safety net. See divergences.

### Decisions that diverged from the design

The four review attentions resolved as follows: **(1) lockfile auto-apply** — auto-apply with no human in the loop, but only after the tripwire result-gate landed, staged via a compile constant (not the "first N regens reviewed" option); **(2) T2253 sequencing** — moot: P1/P2 had already landed in #1799, so the "external" dependency was internal and satisfied (discovering this is what unblocked T2562 after six prior dispatch attempts each produced an empty commit); **(3) direction-2 scope** — both halves in v1; **(4) rung-2 cap** — single-file, yes.

Beyond the attentions:

- **Escalation-on-rejection was replaced by halt-for-sign-off** for deletion-tripwire hits on rungs 0/1 — a deliberate strengthening (#2001). The design's "climbs to rung 3 with findings attached" survives only for _declines_; a rejected _pushed_ resolution rides the ordinary ci_watch/review machinery like any PR head.
- **No rung-specific build gate.** T9's build-gate half was satisfied by observing that the PR's own CI already covers any pushed head regardless of which rung produced it. Correct, and cheaper than designed.
- **`RUNG0_APPLY_LIVE` as a compile-time constant, not a runtime flag** — deliberate ("the gate's call shape isn't decided yet", #1983), but it meant the on-switch required a reviewed code PR (#2043) and the off-state was invisible to runtime introspection. This choice is half of theme 3.
- **Rung ordering as-built:** rung 1 runs first and rung 0 consumes its residue. The design said this in a parenthetical ("rungs 0 and 1 interleave in practice"); as-built it is the actual control flow, stated here because the ladder table reads as "rung 0 then rung 1."

### The six days of hardening (2026-07-14 → 07-19)

Fifteen fix rows plus satellites, all filed outside the project. Grouped by what they revealed.

**Rung 0's three eras of zero.** Why did the design's highest-conviction feature score zero live successes for four days? Three successive causes, each masking the next:

- **Era A — gated off, silently (T2736 → #2043, 07-15).** Rung 0 landed behind `RUNG0_APPLY_LIVE = false` awaiting T9; T9 landed the same day and explicitly did not flip it; nobody flipped it for another day, and nothing alarmed. Worse, with the outer feature flag off, the ladder's skip was logged at debug level — invisible in production traces. The tell, per #2043's own account, was a lockfile-only re-conflict (chore T2680) — the exact case rung 0 exists to absorb for free — that spawned a second full LLM revision (T2729), with `engine-trace.jsonl` showing "no decision line anywhere explaining why." (Correction at this postmortem-of-the-postmortem pass: #2043's body cites this incident as PR #2032, but #2032 in the current PR record is an unrelated crate-extraction PR, and searching by the cited branch name resolves to the same unrelated PR — the incident is corroborated by #2043's own root-cause narrative but its specific PR citation does not check out and should be treated as unverified.) #2043 flipped the constant and added the canonical `conflict_ladder: routing verdict` INFO line at every terminal decision point, including the flag-off skip.
- **Era B — the descriptor suffix (T2816 → #2080, 07-16).** Once live, rung 0 still could not match a single resolver: residual paths came from `jj resolve --list`, whose entries carry a trailing conflict-type descriptor (`MODULE.bazel.lock    2-sided conflict`). `applies_to` compared filenames against the whole annotated string, so no resolver ever matched, and the all-or-nothing rule declined every batch. A lone `MODULE.bazel.lock` conflict on flunge escalated to an LLM agent over, effectively, whitespace. Fix: strip the descriptor at the producer boundary.
- **Era C — the environment (T2830/T2831 → #2091/#2093, 07-16; T2839 → #2098, 07-17).** With matching fixed, two infrastructure classes remained. **(C1)** A transient cube dirty-reclaim lease refusal was treated as a terminal rung-1 outcome, escalating straight to a full agent revision — when the same workspace leased successfully **3 seconds later**. Compounding it, cube's dirty-reclaim guard hard-failed the entire lease call (violating its own "no pool state is ever a hard stop" invariant), and the retire path left the spawned revision row active forever — a phantom "in revision" badge. Fixes: one lease retry plus a new `LadderOutcome::MechanicalRungsUnavailable` that spawns nothing and lets the next pass retry (#2093); quarantine-and-provision-fresh on the cube side (#2091); archive moot revision rows on retire (#2093). **(C2)** The bazel lockfile resolver failed environmentally in **every cold cube workspace**: `bazel mod deps --lockfile_mode=update` re-evaluates the `swift_deps` extension, which needs the gitignored `GhosttyKit.xcframework` that a fresh workspace never has. Fix: the shared stub script wired into `.cube/setup.yaml` (and CI de-duplicated onto it), plus the truthful `ResolveOutcome::Failed { reason }` so an environment failure stops masquerading as "no resolver applies" (#2098; #2095 further split declines into matched-and-declined vs. no-resolver-applies).

**Wedges — attempt states the design never enumerated.** #2009 (07-14): retirement accepted `mergeable=UNKNOWN` as success on a no-contribution path, recording a `succeeded` attempt at an un-advanced head; that terminal row permanently occupied the `UNIQUE (work_item_id, base_sha_at_trigger, head_sha_before)` idempotency slot, so the re-arm loop spun "succeeded crz but still CONFLICTING → UNIQUE collision → no-op" every ~6 seconds, forever, on two PRs (#1398, #1764) — and no terminal-update primitive could invalidate a terminal row, so the wedge could not self-heal. #2013 (07-14): after a restart killed a revision, one of the two attempt-discovery branches re-armed the parent unconditionally with no staleness check — up to ~2 minutes of UNIQUE-collision spam per conflict before supersede. #2011 (07-14): engine-spawned revision executions were created with no execution row at all; the dep-unblock sweep's stuck-execution rescue had silently become their de facto scheduler, taxing every engine-triggered revision dispatch with up to 30 seconds of latency.

**Trust-but-verify (the T2764/#2023 lineage → #2057 07-15, #2059 07-16).** Adjacent machinery, same season, same lesson. A worker rebased a bookmark but never edited into it: the local bazel gate validated `main`'s tree while `cube pr update` pushed the never-compiled rebased commit — "only red CI stopped an armed auto-merge" (#2057's fix: refuse to push when the working copy is not the bookmark head). And `mark-succeeded-via-rebase` flipped CI-remediation attempts to `succeeded` purely on the worker's say-so — in the incident, 40 seconds before Buildkite even started on a head that then failed all three required checks (#2059's fix: the engine independently probes live CI on the current head before honoring the claim). Continuation of the T581/T2453 lineage: any worker claim a verifier can check, the engine must check.

**The environment, again — checkleft's Node (T2918 → #2132, 07-19).** Every engine-context push through the checkleft push gate died because `npx` resolved to a stale root-owned `/usr/local/bin/node` v20.7.0 that crashed `oxfmt` — checkleft pinned the npm _package_ version but took whatever _runtime_ the ambient PATH offered. The code fix is a preflight gate (Node ≥ 22, actionable diagnostic, fallback honored) plus a spawn-failure misattribution fix; full hermeticity was explicitly deferred ("requires new infra this repo lacks"). The actual unblocking was operational: installing Node v24.8.0 on 07-19. Until then, note the compounding: rung 0's push path ran the checkleft gate, so even a perfect resolver could not have pushed from engine context.

**The 2026-07-18 restart incident (T2919/T2920/T2921 → #2136/#2134/#2135, all 07-19).** The sharpest single lesson. Mechanical rungs run inline in the engine process — no dispatched worker, no execution row, no pane, no registry entry. When the engine restarted (a ~2-second gap) mid-rung-0 on flunge #960's conflict: **(a)** the `conflict_resolutions` row was left non-terminal with nothing to recover it — the attempt "vanished with no verdict, no error, no re-queue," and the re-arm path treated the pending row as an old-style in-flight attempt, declining to dispatch forever (#2136: durable `mechanical_rung_in_flight` marker + boot-time `reconcile_orphaned_conflict_ladder_attempts`); **(b)** its cube lease sat orphaned to the dead engine until cube's 30-minute TTL sweep, because no lease machinery knew inline rung leases existed (#2134: an in-memory lease registry drained on both shutdown paths, an install-id-scoped startup reap with `kill(pid,0)` corroboration, and a 600s/120s heartbeat as belt-and-braces); **(c)** the conflict was never re-detected because revision liveness read only the task's `status` column — paper liveness that survives restarts — while the helper's own doc comment claimed to cover "its execution died outright (e.g. an engine restart)" (#2135: liveness now consults execution death terminals, so the first post-boot pass supersedes and respawns). The conflict itself was cleared by a manually filed worker revision and flunge #960 merged the next day.

**Satellites.** #1986 pinned resolver dispatch order ("first matching resolver in registration order wins" was previously untested) and the telemetry class labels; #2079 added direct tests for the conflict-resolution DB primitives; #2106 split the 2,885-line `conflict_watch_tests.rs` before it crossed the 3,000-line file-size check; #2139 removed an unused import. The #1995/#1996 duplicate-chore pair is covered above under frequency (T2944 context).

### What the design failed to anticipate — six themes

1. **No lifecycle for in-flight attempts.** The design specified rungs, gates, and outcomes — a decision procedure — and implicitly assumed every attempt runs to a verdict. Inline engine-side execution has no such guarantee: the engine restarts on every deploy. The 07-18 trio plus #2013/#2009 are all one missing design sentence: _an attempt is a durable object with crash semantics — in-flight state is recorded, orphans are reconciled at boot, every terminal state frees the idempotency slot, and liveness is measured against executions, not status columns._
2. **"Deterministic" ≠ hermetic.** The central bet — formula beats agent — was right about the formula and silent about the formula's _environment_. `bazel mod deps` needs a gitignored xcframework; the push gate needs a modern Node; the registry needs bare paths, not jj's annotated ones. Three independent environmental assumptions each produced a 100% failure rate in exactly the automated contexts (cold workspaces, engine-context pushes) that never occur on a developer's machine. A deterministic resolver's contract must include its environmental preconditions — provisioned (setup.yaml stubs) or preflighted (version gates) — and an environment failure must surface as `Failed`, never as a decline.
3. **The measurement project shipped its own mechanism dark.** This doc says "measurement is a deliverable, not an afterthought" — yet the ladder's own decision points were uninstrumented: the flag-off skip logged at debug, the compile gate invisible at runtime, resolver invocations traceless, and a 0-of-42 lifetime success count alarming no one for days. The design measured _conflicts_ diligently and its _own feature_ not at all. Anything shipped dark needs an INFO-level decision line at every terminal branch from day one, and an expiry on the darkness — a gate without an alarm becomes a forgotten switch.
4. **Infrastructure failure conflated with genuine decline.** The ladder's outcome vocabulary (resolved / declined / fell-through) had no arm for "the ladder could not run here." So a 3-second lease hiccup bought a full LLM revision, and a missing xcframework read as "not a formulaic conflict." An escalation ladder needs _unavailable_ as a first-class outcome with retry-in-place semantics, distinct from _declined_, which escalates.
5. **Reporting fidelity is a correctness surface.** Rung-1 residue enumeration looked only at the working-copy tip while the push refuses the whole range for any conflicted ancestor; decline logs blamed "no resolver" for resolver-ran-and-declined cases. Downstream routing — rung-2 eligibility, diagnosis paths — consumed those reports as truth. Where a report drives routing, its fidelity is load-bearing and deserves tests as such.
6. **Success claims need verifiers.** The system repeatedly trusted a worker's assertion — "the tests passed," "CI is green" — where an independent check was cheap (#2057, #2059; older lineage T581/T2453). The engine now verifies that the tree pushed is the tree tested, and probes live CI before honoring a success claim. The worker-proposal-API design generalizes the lesson: worker statements become typed, validated submissions rather than trusted prose.

### What went well

- **Failed closed, every time.** Across the whole family the failure modes were exactly two: silent fall-through to the expensive-but-correct rung-3 worker (cost and latency, never correctness), or a wedge that left a conflict stranded-but-safe. No ladder defect ever pushed a wrong resolution; the T2253 tripwire never had to catch a bad mechanical merge because none was produced. The all-or-nothing rung-0 rule and one-shot-then-escalate structure are why every bug's worst case was money, not corruption. (The one near-miss of the window — the armed auto-merge stopped only by red CI — was in adjacent CI-remediation machinery, not the ladder, and produced #2057/#2059.)
- **The safety-first sequencing did its job.** Rung 0 stayed dark until the tripwire result-gate existed, exactly as the attentions resolution intended. The failure was the missing alarm on the darkness (theme 3), not the gate itself.
- **The architecture absorbed every fix.** All fifteen rows landed inside the designed seams — new `ResolveOutcome`/`LadderOutcome` variants, new columns on `conflict_resolutions`, lease machinery beside the ladder — with no re-architecting. The registry shape held: the append-union resolver and the recipe interpreter dropped in with zero registry API change, as designed.
- **Diagnosis velocity.** Every defect was root-caused with live-incident evidence (trace lines, lease ids, second-level timestamps) and fixed within one to two days of surfacing — largely by the same worker fleet the ladder exists to serve.

### The merge-queue reversal (P2951), reconciled

This design rejected merge queues (Alternatives, B): serialization at the moment of highest contention, no frequency reduction, still needs a resolver. Six days later, flunge adopted the Trunk merge queue (P2951; design #2150, [`trunk-merge-queue-integration`](trunk-merge-queue-integration-queue-backed-merges-merging-ui.md)).

What actually happened: on 07-19 flunge hit a **semantic merge race** — PR #963 added a required field while #964/#965 concurrently added call sites without it; each PR was green on its own base, the merged combination was compile-broken (`E0063`) on main, and the breakage was then masked by path-filtered CI (flunge #982's account, from the corpus-replay validation sweep). A 07-13 precedent had the same shape: backend #818 shifted a wire type, web #820 merged 14 minutes later and silently dropped a field at decode. This class — _individually green, jointly broken, zero textual conflict_ — is invisible to every rung of this ladder **by construction**: there is nothing to resolve. The rejection above argued frequency, throughput, and resolution cost; it never considered semantic races, because this design is about conflicts and a semantic race is not one.

So the two positions do not collide, and the as-built systems compose explicitly: **the ladder resolves textual conflicts before and during review; the queue validates the merged combination before it lands.** #2150 encodes the composition concretely — a conflict detected mid-queue is cancelled out of the queue and handed to `conflict_watch` ("the conflict resolver owns the slot," matching the existing conflict-pre-empts-CI precedence), and queue evictions ride the existing ci_watch remediation machinery as a new `failure_kind`.

Two honest caveats, recorded so this reconciliation is not retconned into either doc. First, neither #2150's body nor its design doc mentions the semantic race or this design's rejection — the adoption rationale lives in the flunge repo, and this postmortem is the first document connecting the two positions. Second, the rejection's throughput argument was never rebutted, only narrowed: P2951 is a flunge-only **trial** (mono explicitly stays on direct merges), and whether queue serialization costs real throughput at mono's parallelism remains an open question this design's rejection still owns.

### Current state (as of 2026-07-19)

- **Live** (where the operator has enabled the deliberately default-off `conflict_ladder_mechanical_rebase` flag): rung-1 engine-direct rebase; rung-0 apply (`RUNG0_APPLY_LIVE = true` since #2043) with the `cargo_lock`, `bazel_module_lock`, and `registry_append_union` resolvers via `ResolverRegistry::with_builtins()`; rung-2 single-file Sonnet/`trivial`; tripwire halt-for-sign-off on rungs 0/1; restart survival (durable in-flight marker, boot reconcile, lease registry/reap/heartbeat).
- **Shipped but dark:** speculative prediction (`speculative_conflict_prediction`, default off) and stacked-PR structuring offers (`stacked_pr_auto_structuring`, default off; the accept-and-convert step is unbuilt, blocked on auto-rebase machinery that has not shipped).
- **Shipped but unwired:** declarative recipes — `RecipeResolver`/`ConflictRecipesStore` (#1975) has no live consumer; `attempt_rung0` builds `with_builtins()` only and nothing loads `conflict-recipes.toml`.
- **Deferred with a real cost:** rung-2 pre-staging (a second lease+goto+rebase per rung-2 spawn); checkleft Node hermeticity (preflight diagnostic only; the push gate depends on an operationally installed Node ≥ 22).
- **Unknowns the telemetry should answer next:** the post-fix rung-0 live success rate (no confirmed live success in the record yet), the true conflict rate vs. the ~12% floor, and whether `merge_order` and hotspot refactoring move frequency at all.
