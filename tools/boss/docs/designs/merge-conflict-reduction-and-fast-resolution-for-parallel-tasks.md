# Boss: Merge-Conflict Reduction and Fast Resolution for Parallel Tasks

- **Status:** Design — for review. No code in this PR.
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
