# Why merge conflicts are so frequent in mono, and what to do about them

- **Date:** 2026-11-11
- **Kind:** investigation (read-only). No source, build, CI, GitHub or engine settings were changed.
- **Scope:** PRs against `main` in `spinyfin/mono` from 2026-05-01 to 2026-11-11, the period the engine's conflict ledger covers. Repository revision `ca283c0224db`.
- **Data:** the engine's conflict attempt ledger (1,767 rows), GitHub metadata and timelines for all 3,388 PRs, and a throwaway clone holding every PR head plus 2,267 force-pushed-away commits. Every flagged conflict was re-merged with `git merge-tree` to recover the real conflicted files and hunks. See [How this was measured](#how-this-was-measured).
- **Related:** [conflict reduction and fast resolution design](../../tools/boss/docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md), [conflict handling in review](../../tools/boss/docs/designs/merge-conflict-handling-in-review.md), [auto-rebase for stacked PRs](../../tools/boss/docs/designs/auto-rebase-stacked-prs.md), [incident 002](../../tools/boss/docs/postmortems/incident-002-merge-conflict-deletion-blessed-by-review.md), [database migrations](../../tools/boss/docs/database-migrations.md), [mono merge queue investigation](../../tools/boss/docs/investigations/mono-merge-queue-github-to-trunk-2026-10-14.md).
- **Evidence labels:** **[measured]** computed from the data above. **[code]** read in this checkout at the cited path. **[estimate]** derived from measured numbers under a stated assumption. **[speculation]** not tested here.

The share of merged PRs that hit at least one merge conflict while in review rose from 7% in May to 34% in early November. This document measures where those conflicts come from, how the engine's resolution pipeline performs, and which changes would reduce them or make them cheap.

## Verdict

**Conflicts follow exposure: how many PRs are open each time something merges.** Count one pair for every PR that is open at the moment another PR merges. Since July the ledger shows a steady 0.8 to 0.9 in-review conflicts per 100 such pairs. Open PRs went from a median of 4 in May to about 100 now, the median PR lifetime from 6 minutes to 10 hours, and the median PR from 3 files to 11 or more. With 100 PRs open, every merge to `main` breaks roughly one of them.

**Nearly half of the conflicts need no judgement.** Of 1,233 distinct conflict states, 45% consist only of a lockfile to regenerate or of hunks where both sides inserted new lines at the same spot. A lockfile, almost always `MODULE.bazel.lock`, is the only conflicted file in 16%. The schema migration chain is the only cause in a further 8% and wrapped import lists in 5%.

**The cheap resolution tiers almost never fire.** Of 760 successful resolutions since the escalation ladder shipped, 14 (1.8%) finished without an agent. Since the mechanical tiers became default-on, 21 of 27 resolved lockfile-only conflicts still went to an agent.

**A bug hides the conflict from the engine's own diagnosis.** The pre-spawn diagnosis merges the PR head against the PR's recorded base instead of current `main`. It came back empty for 88% of attempts (94% since August), so the resolver's prompt, the conflict classification and the hotspot report all run mostly blind.

**More than half of the resolution work is repeated.** 53% of conflict states are a second or later conflict on a PR that had already conflicted once. 108 PRs are open with a median age of 68 days, and 68 of them conflict with `main` today.

**Resolution time is fixed overhead.** An agent takes a median of 15 to 23 minutes whether the conflict is one hunk or nine, and 10 minutes for a lockfile alone. The low-effort agent tier saves under three minutes over the normal one.

Ranked recommendations, quick wins first:

1. Fix the diagnosis base so the engine sees the real conflict (small change, unblocks everything that depends on classification).
2. Make lockfile-only conflicts actually retire in the deterministic tier, and record on each ledger row why the ladder fell through.
3. Remove the two shared append points left in the schema migration chain, and break up the five giant wrapped import lists.
4. Keep the current `crate.from_cargo` configuration after the [initial lockfile trial](#executed-lockfile-trial): reproducibility moves the two observed conflicts into cargo-bazel's JSON, and repinning is not consistently cheaper. Stripping alone leaves manifest conflicts.
5. Resolve conflicts just in time: stop re-resolving PRs that are not about to merge, and give long-stale PRs a close-or-rewrite decision.
6. Add a keep-both resolver for same-spot insertions, and trial a syntax-aware merge tool as a tier between the engine rebase and an agent.

Details, costs and risks are in [Ideas](#ideas) and [Ranked recommendation](#ranked-recommendation).

## How this was measured

**Sources.**

- **Engine ledger.** `boss engine conflicts list --json` returned 1,767 attempt rows for this product: 1,524 raised by the in-review watcher and 243 self-reported by workers that hit a conflict while rebasing mid-task. Each in-review row carries the PR number, the PR head commit at detection, the detection time, the outcome and the tier that resolved it.
- **GitHub.** One GraphQL pass over all 3,388 PRs (3,188 merged, 108 open, 92 closed) for dates, size, force-push events and merge-queue events.
- **Git objects.** A full clone in a scratch directory, plus `refs/pull/*/head` and the 2,267 commits that force-pushes had replaced, fetched by SHA. All were still retrievable.

**Replay.** For each in-review ledger row I ran `git merge-tree --write-tree` (diff3 conflict style) between the recorded PR head and `main` as it stood at the detection time. The result lists the conflicted files and writes the conflict hunks, which I parsed and classified. A second pass found the first `main` commit after which the merge conflicts (the "onset"), and a third simulated a commit-by-commit rebase.

**Validation.** [measured]

- 1,498 of 1,524 in-review rows (98.3%) reproduce as a conflict against `main` at detection time.
- Independently, GitHub shows 1,644 force-pushes that moved a PR onto a newer `main`. 1,093 of the pre-push heads conflict when replayed, and 1,029 of those (94%) are in the ledger. The two sources agree.

**Unit of analysis.** A _conflict state_ is one distinct (PR, head commit) that conflicted. The 1,524 rows collapse to 1,233 conflict states on 575 PRs, covering 1,940 conflicted file rows in 488 distinct files and 2,630 hunks outside lockfiles.

**Limits.**

- The hunk classification is a heuristic over hunk text. I checked about twenty hunks by eye, not a formal sample.
- Conflicts that a worker resolves before its first push leave no trace on GitHub. Only the 243 self-reported rows cover them, and that reporting is incomplete (see [Telemetry gaps](#telemetry-gaps)).
- Agent cost per resolution, CI minutes per rerun and the ladder's fall-through reasons live in engine state and logs I did not read. They are listed under [What was not measured](#what-was-not-measured).
- The work tracker count taken on 2026-11-11 is 815 conflict revisions since May. The ledger links 1,374 distinct revision ids over the same period, and I could not reconcile the two. The tracker's monthly counts are close to the ledger's succeeded rows from June to August (151, 283 and 108 against 153, 290 and 113) and well below them in October and November (123 and 39 against 246 and 77).

## Findings

### 1. Scale and trend

[measured] Monthly figures for PRs against `main`. November covers the first 11 days.

| Month | Merged PRs | Merges per day | Median PR lifetime | 90th pct lifetime | Median open PRs | Median files per PR | Conflict states | Merged PRs with a conflict |
| ----- | ---------: | -------------: | -----------------: | ----------------: | --------------: | ------------------: | --------------: | -------------------------: |
| May   |        776 |           25.0 |              0.1 h |             3.6 h |               4 |                   3 |              80 |                         7% |
| Jun   |        511 |           17.0 |              0.3 h |            18.6 h |              13 |                   3 |             160 |                        18% |
| Jul   |        887 |           28.6 |              0.8 h |            18.5 h |              43 |                   4 |             354 |                        17% |
| Aug   |        212 |            6.8 |              7.5 h |            87.6 h |              69 |                   7 |             125 |                        20% |
| Sep   |        188 |            6.3 |              6.6 h |            94.4 h |              68 |                   8 |             105 |                        22% |
| Oct   |        352 |           11.4 |             10.0 h |            88.1 h |              96 |                  11 |             303 |                        25% |
| Nov   |        120 |           10.9 |              3.7 h |            50.3 h |             101 |                  19 |             106 |                        34% |

PR lifetime runs from creation to merge. Open PRs are counted at every PR open and close, and the month's median is shown.

Across the whole period 514 of 3,046 merged PRs (17%) conflicted in review at least once, and those PRs averaged 1.8 conflict states each.

91% of conflict states involve only files under `tools/boss/` plus the two root lockfiles. `tools/checkleft/` appears in 71 states and `tools/cube/` in 17.

### 2. The driver is exposure: open PRs times merges

[measured] Count one pair for every PR that is open at the moment another PR merges. Dividing each month's conflict states by that month's pairs gives a nearly constant rate from July on.

| Month | (Open PR, merge) pairs | Conflict states | States per 100 pairs |
| ----- | ---------------------: | --------------: | -------------------: |
| May   |                  3,289 |              80 |                 2.43 |
| Jun   |                  6,977 |             160 |                 2.29 |
| Jul   |                 37,881 |             354 |                 0.93 |
| Aug   |                 14,513 |             125 |                 0.86 |
| Sep   |                 12,747 |             105 |                 0.82 |
| Oct   |                 33,517 |             303 |                 0.90 |
| Nov   |                 12,145 |             106 |                 0.87 |

The rate held at 0.8 to 0.9 whether `main` moved at 29 merges a day (July) or 6 (September). The pair count includes long-stale open PRs, which dilute the rate. Restricted to PRs under 14 days old it rises from 1.9 in July to 4.4 in November, consistent with PRs getting larger.

The practical reading: conflicts per day are roughly the pair rate times open PRs times merges per day. Open PRs are themselves arrival rate times lifetime, so at a fixed throughput the conflict count scales with how long PRs stay open.

[measured] Incidence by PR lifetime and size, merged PRs since May:

| PR lifetime | PRs   | Conflicted | States per PR |
| ----------- | ----- | ---------: | ------------: |
| under 1 h   | 1,649 |       5.3% |          0.06 |
| 1 to 6 h    | 565   |      20.5% |          0.30 |
| 6 to 24 h   | 484   |      25.8% |          0.38 |
| 1 to 3 days | 210   |      44.8% |          0.90 |
| over 3 days | 138   |      65.9% |          2.22 |

| Files changed | PRs   | Conflicted | States per PR |
| ------------- | ----- | ---------: | ------------: |
| 1 to 2        | 1,028 |       5.2% |          0.07 |
| 3 to 5        | 746   |       9.4% |          0.12 |
| 6 to 10       | 552   |      21.2% |          0.39 |
| 11 to 20      | 419   |      31.0% |          0.56 |
| 21 to 40      | 247   |      45.7% |          1.09 |
| over 40       | 54    |      57.4% |          1.19 |

Four more observations shape which remedies can work. [measured]

- **Conflicts are immediate, not accumulated drift.** In 73% of conflict states exactly one `main` commit since the branch point touched the conflicted files. The branch was a median of 6 commits behind `main` at detection (90th pct 31). Rebasing more often would not have avoided these: the conflict exists from the moment the other PR merges.
- **No small set of PRs is to blame.** 694 distinct `main` commits triggered a conflict. The top 10 account for 6% of conflict states and the top 100 for 34%. The triggering PRs are larger than average (median 14 files against 4), but they are ordinary feature work.
- **Stacked PRs and renames are not a source.** 13 of 3,388 PRs targeted a branch other than `main`. 96% of conflicted file rows are content conflicts; add/add, modify/delete and rename conflicts together are 80 of 1,940, even though the engine was being split into crates during the period.
- **Replaying commit by commit adds little.** 62% of conflicted branches had more than one commit, but a simulated per-commit rebase needs 1.18 conflicted commits resolved per state, and it touches a file outside the final-tree conflict in 8 of 1,215 states.

**Standing inventory.** [measured] 108 PRs against `main` are open. Their median age is 68 days, 96 are older than a week, and 68 conflict with `main` as of this revision. The 44 open PRs that appear in the ledger account for 235 conflict states (19% of the total) and 17 closed-unmerged PRs for another 57. One PR (#2070) has been open 119 days and has conflicted 34 times.

**Recurrence.** [measured] 658 of 1,233 conflict states (53%) are a second or later conflict on the same PR. 73% of those repeats hit a file that had already conflicted on that PR. On PRs that eventually merged, 427 of 941 conflict states were followed by another conflict before the merge. After a resolution, the median time to the next conflict on the same PR is 9.5 hours; the median time from the last resolution to the merge is 0.9 hours (75th pct 8.7 hours).

### 3. Where conflicts land

[measured] Top files by conflict states. "Sole" counts states where the file was the only one conflicted. "Yield" is conflict states per `main` commit that touched the file since May: a yield near 1 means nearly every merge that touches the file breaks an open PR.

| File                                                             | States | Share | Sole | Yield |
| ---------------------------------------------------------------- | -----: | ----: | ---: | ----: |
| `MODULE.bazel.lock`                                              |    260 | 21.1% |  193 |  0.99 |
| `tools/boss/engine/core/src/work/schema_init.rs`                 |     95 |  7.7% |   70 |  0.65 |
| `tools/boss/engine/core/src/app.rs`                              |     71 |  5.8% |   48 |  0.27 |
| `tools/boss/protocol/src/wire.rs`                                |     38 |  3.1% |   19 |  0.19 |
| `tools/boss/engine/core/src/metrics_init.rs`                     |     33 |  2.7% |   17 |  0.92 |
| `tools/boss/engine/core/src/work.rs`                             |     30 |  2.4% |   20 |  0.17 |
| `tools/boss/engine/core/src/coordinator.rs`                      |     28 |  2.3% |   13 |  0.16 |
| `tools/boss/engine/core/src/lib.rs`                              |     26 |  2.1% |   14 |  0.13 |
| `tools/boss/engine/core/src/completion.rs`                       |     24 |  1.9% |   14 |  0.14 |
| `tools/boss/engine/work-db/src/schema_init.rs`                   |     23 |  1.9% |   11 |  0.92 |
| `tools/checkleft/src/external/runtime.rs`                        |     18 |  1.5% |    7 |  0.45 |
| `tools/boss/engine/core/src/runner.rs`                           |     17 |  1.4% |    6 |  0.16 |
| `tools/boss/engine/core/src/runner/prompt.rs`                    |     17 |  1.4% |   11 |  0.26 |
| `tools/boss/app-macos/Sources/Ghostty/WorkersDetailView.swift`   |     17 |  1.4% |    9 |  0.35 |
| `tools/boss/engine/core/BUILD.bazel`                             |     16 |  1.3% |    5 |  0.10 |
| `tools/boss/engine/driver/src/lib.rs`                            |     16 |  1.3% |    3 |  0.22 |
| `Cargo.lock`                                                     |     15 |  1.2% |    1 |  0.07 |
| `tools/boss/engine/core/src/app/server.rs`                       |     15 |  1.2% |    8 |  0.09 |
| `tools/boss/engine/core/src/host_adapter.rs`                     |     14 |  1.1% |    2 |  0.14 |
| `tools/boss/app-macos/Sources/ChatViewModel+EventHandling.swift` |     14 |  1.1% |    8 |  0.18 |

Paths are as they were when the conflict happened; several of these files have since moved into extracted crates. Counted by file name, `schema_init.rs` has 122 states, `lib.rs` 94, `app.rs` 83 and `BUILD.bazel` 66.

**Concentration.** [measured] 72% of conflict states involve exactly one file. The top 5 files appear in 39% of states and are the only conflicted files in 29%. The top 20 appear in 57% and are the only files in 42%. Past that there is a long tail: 488 files, and 1,237 distinct (file, enclosing function or type) regions of which the top 100 hold 37% of hunks.

**Two contrasts worth noting.** `Cargo.lock` is touched by 214 `main` commits and conflicts 15 times (yield 0.07), because it is sorted by package and merges well. `MODULE.bazel.lock` is touched by 263 and conflicts 260 times.

### 4. Why those regions conflict

[measured] Conflicted file rows by kind:

| Kind                       | File rows | Share |
| -------------------------- | --------: | ----: |
| Rust source                |       923 | 47.6% |
| Lockfiles                  |       275 | 14.2% |
| Tests                      |       206 | 10.6% |
| Schema and migration files |       154 |  7.9% |
| Swift app                  |       120 |  6.2% |
| `mod.rs` and `lib.rs`      |       106 |  5.5% |
| Build files and manifests  |        89 |  4.6% |
| Docs, config and other     |        67 |  3.5% |

[measured] The 2,630 hunks outside lockfiles, by what the two sides did:

| Hunk shape                                       | Hunks | Share |
| ------------------------------------------------ | ----: | ----: |
| Both sides changed the same lines                | 1,051 | 40.0% |
| Both sides inserted new lines at the same spot   |   891 | 33.9% |
| One side inserted, the other edited nearby lines |   491 | 18.7% |
| One side deleted lines the other changed         |   192 |  7.3% |
| Formatting only                                  |     5 |  0.2% |

Reading the hunks in the hot regions shows a handful of recurring mechanisms. Each is a place where two unrelated changes are forced onto the same lines.

**`MODULE.bazel.lock` records a hash of every Cargo manifest.** [measured] [code] Recounting the retained corpus finds Cargo input lines in 255 of its 260 conflict states (the original 254-count subset and its additional state are detailed in S1). These include `"FILE:@@//Cargo.lock <sha256>"` and `"FILE:@@//tools/boss/engine/core/Cargo.toml <sha256>"` inside the `crate_universe` extension's record. PRs that touch the same manifest, a shared workspace manifest or `Cargo.lock` can conflict on these hashes even when the inputs merge cleanly; changes to different manifests can also collide on adjacent input entries or generated dependency maps. The pinned `rules_rust` 0.70.0 marks the extension non-reproducible when `from_cargo` has no `lockfile` attribute, which is mono's configuration (`MODULE.bazel:338-341`), so Bazel stores the full evaluation in the lockfile. The share of `main` commits touching `MODULE.bazel.lock` was 5% to 11% from May to October and 25% in November, as crate extraction added manifests (126 `Cargo.toml` files today). The BazelCon proposal addresses one of these inputs; see [the concrete evaluation](#s1-evaluating-external-only-cargo-lockfiles).

**The schema migration chain has shared append points.** [measured] [code] Its hunks sit in four places: the migration steps in `fn init` and `fn run_full_migration_chain`, a test that lists every migration, and the compatibility floor constant. Replacing integer schema versions with named migrations (#3360, 2026-10-29) retired the `if version < N` steps and the id collisions, but not the lists. Today `schema_migration_registry.rs` holds one array where new entries go at the top (`tools/boss/engine/work-db/src/schema_migration_registry.rs:15-17`), and the test `supported_databases_apply_post_floor_migrations_without_losing_data` lists 36 `migrate(&expected)` calls with new ones appended at the end (`tools/boss/engine/work-db/src/schema_init.rs:731-801`). Two PRs that each add a migration insert at the same line in both files.

**Wrapped import lists reflow on every addition.** [measured] 180 hunks (6.8%) are inside brace-wrapped `use` lists. rustfmt packs names to the 120-column width, so adding one name shifts every later line and collides with any other addition. Five lists carry most of this: `pub use boss_protocol::{…}` in `tools/boss/engine/work-db/src/lib.rs:359` (157 names on 34 lines), `use crate::types::{…}` in `tools/boss/protocol/src/wire.rs:13` (99 names), and three more with 46 to 79 names in `tools/boss/cli/src/main.rs`, `tools/boss/cli/src/tests.rs` and `tools/boss/engine/app/src/app.rs`.

**A hand-maintained total.** [measured] `metrics_init.rs` had a test asserting the number of registered counters, so every PR that added a counter edited the same line. This was replaced with a sorted list of names in #3124 (2026-10-11).

**Same-spot insertions.** [measured] 891 hunks are both sides adding something new at one position. Outside the schema chain and the import lists, what they insert is statements (202), a new function or test at the end of the same module (178), enum variants or struct fields (118), `mod` and `use` declarations (63), build file list entries (49) and match arms (42). `enum FrontendRequest` is kept alphabetical to spread insertions, but verbs cluster by prefix: observed pairs include `GetStuckReport` against `GetSolComparisonReport` and `GetOffsiteBackup` against `GetMetricCatalog`.

**Shared hub types.** [measured] The remaining concentration is in types every subsystem extends: `struct ServerState` and its constructor, `fn handle_frontend_connection`, `struct ExecutionCoordinator` and `fn run_execution`. These are real concurrent edits to shared logic.

**How much is mechanical.** [measured, heuristic classification] Each conflict state is assigned to the hardest hunk it contains.

| Conflict state consists of                                                      | States | Share |
| ------------------------------------------------------------------------------- | -----: | ----: |
| Lockfiles only: regenerate                                                      |    198 | 16.1% |
| Same-spot insertions of whole items or import-list entries: keep both           |    167 | 13.5% |
| Same-spot insertions of statements or list entries: keep both, order may matter |    193 | 15.7% |
| At least one hunk where the two sides changed the same lines differently        |    675 | 54.7% |

So 45% of conflict states are trivially mechanical in the sense asked: a regeneration or a keep-both union. By named cause, lockfiles alone explain 198 states, the schema chain alone 99, import lists alone 59 and the metrics test alone 17; 385 states (31%) are explained entirely by some combination of those four.

**Two natural experiments.** [measured] Yield before and after earlier fixes:

| Change                                                       | Before                    | After                     |
| ------------------------------------------------------------ | ------------------------- | ------------------------- |
| Counter total replaced by a sorted name list (#3124)         | 28 states per 28 touches  | 5 states per 14 touches   |
| Integer schema versions replaced by named migrations (#3360) | 97 states per 155 touches | 32 states per 38 touches  |
| `FrontendRequest` variants sorted (2026-07-13)               | 21 states per 103 touches | 17 states per 102 touches |

The sorted name list cut the yield by about two thirds. The named-migration change did not reduce textual conflicts, because the lists stayed. Sorting `FrontendRequest` shows no clear effect on `wire.rs` as a whole, which is dominated by its import list.

### 5. How the pipeline performs today

**What it does.** [code] Paths below are under `tools/boss/engine/`.

1. The merge poller sweeps every 60 seconds and treats GitHub's `mergeable = CONFLICTING` as a conflict (`merge-poller/src/classify.rs:547`, `app/src/app/server.rs:1767`).
2. Each detection inserts a ledger row keyed on work item, recorded base commit and head commit (`core/src/conflict_watch.rs:752-774`).
3. With `conflict_ladder_mechanical_rebase` on (default since 2026-10-28, `feature-flags/src/lib.rs:200-207`) the engine first rebases the branch itself, then runs deterministic resolvers on what is left: lockfile regeneration, and a union of `mod`/`use` lines that applies only to files named `mod.rs` or `lib.rs` (`deterministic-resolvers/src/resolvers/registry_append_union.rs:54-58`).
4. Otherwise it creates a "Resolve merge conflict against main" revision (`core/src/conflict_watch.rs:1524`). One leftover file gets a low-effort agent; more get a normal one (`core/src/conflict_ladder.rs:90`, `core/src/conflict_watch.rs:1532`).
5. A fourth attempt on one PR within an hour is abandoned and the PR is left for a human (`work-db/src/lib.rs:106-110`).

Prediction of conflicts by speculative rebase and offers to stack overlapping PRs exist but are default-off (`feature-flags/src/lib.rs:209-251`). Planner merge-order hints never gate dispatch (`core/src/planner.rs:839`). I found nothing that rebases an open PR before GitHub reports a conflict. mono already lands through GitHub's merge queue.

**Outcomes.** [measured] The 1,524 in-review attempt rows:

| Outcome                                                    |  Rows | Share |
| ---------------------------------------------------------- | ----: | ----: |
| Succeeded                                                  | 1,038 | 68.1% |
| Abandoned: head moved before the attempt finished          |   348 | 22.8% |
| Abandoned: fourth attempt within an hour, left for a human |    58 |  3.8% |
| Abandoned: other                                           |     8 |  0.5% |
| Failed: stop condition or no push                          |    31 |  2.0% |
| Pending at export                                          |    41 |  2.7% |

The 31 failures are 8 suspected obsolescence, 6 architectural mismatch, 6 product decision required, 5 marked resolved but still conflicting, 5 without a push, and 1 unlabelled. About 6% of attempts end with a human needed.

**Who resolved it.** [measured] Of the 760 successes since the ladder shipped in mid-July:

| Resolved by                      | Successes | Median time from detection | 90th pct |
| -------------------------------- | --------: | -------------------------: | -------: |
| Engine rebase, no agent          |         7 |                    0.2 min |    1 min |
| Deterministic resolver, no agent |         7 |                    5.6 min |   34 min |
| Low-effort agent                 |       525 |                   15.0 min |   87 min |
| Normal agent                     |       221 |                   17.6 min |  114 min |

Since the mechanical tiers became default-on (2026-10-29 onward) there were 35 lockfile-only conflict states: 6 retired in the deterministic tier, 21 were resolved by an agent, 5 were abandoned and 3 are pending. The ledger does not record why the ladder fell through for the 21.

**Latency.** [measured]

- From the `main` commit that caused the conflict to detection: median 6.4 minutes, 90th pct 45 minutes. 71% are detected within 10 minutes.
- From detection to cleared, successful states: median 11.7 minutes, 90th pct 63 minutes, with a long tail. By month the median rose from 4.7 minutes in May to 26.1 in October and was 11.2 in November.
- PRs spent a total of about 2,400 hours in a conflicted state before a successful resolution.

**Time does not track difficulty.** [measured] Agent-resolved states by size:

| Conflict         | States | Median |
| ---------------- | -----: | -----: |
| Lockfile only    |    121 | 10 min |
| 1 hunk           |    291 | 16 min |
| 2 to 3 hunks     |    155 | 15 min |
| 4 to 9 hunks     |     77 | 23 min |
| 10 or more hunks |     13 | 48 min |

The cost of a resolution is dominated by what surrounds the merge: queueing, leasing and positioning a workspace, the rebase, the build and test gate, and the push. Each successful resolution is also a force-push that starts a new CI run. GitHub shows 753 force-pushes that changed nothing except the conflicted files.

**The diagnosis runs against the wrong base.** [code] [measured] Before spawning a resolver the engine runs `git merge-tree` and stores the result as the attempt's diagnosis, which is rendered into the resolver's prompt and feeds the conflict class. The two commits it merges are the attempt's `head_sha_before` and `base_sha_at_trigger` (`dispatch/src/coordinator/run.rs:924-934`). The second is GitHub's `baseRefOid`, which a comment at the insert site describes as fixed when the PR opens and not tracking `main` (`core/src/conflict_watch.rs:756-758`). In the ledger it is a median of 4 `main` commits behind `main` at detection (90th pct 35), and in 607 of 1,199 rows it is an ancestor of the PR head, so the merge is trivially clean.

- Of 868 stored diagnoses that did not error, 767 (88%) list no files. Since August it is 503 of 535 (94%).
- Replaying against the stored base reproduces the stored result in all 784 rows where both exist. Replaying against `main` finds a conflict in 98.3% of rows.
- 724 of 1,025 classified rows have class `unknown`. `boss engine conflicts hotspots` reports `MODULE.bazel.lock` 55 times; the replay finds it in 260 in-review states.
- The design postmortem's observation that 91 of 107 attempts had "a completely clean `git merge-tree` yet still spawned an agent" is most simply explained by this. I could not reproduce its premise that those branches needed no decision: GitHub's verdicts replay as real conflicts.

#### Telemetry gaps

[code] Workers are told to record a mid-task conflict only after `cube workspace rebase` reports one (`execution-runner/src/worker_setup.rs:440`), but the pre-push instruction tells them to use `cube workspace local-rebase` (`execution-runner/src/prompt_fragments.rs:47`). Conflicts resolved during the mandated pre-push rebase are therefore likely unreported. The 243 self-reported rows are a floor; their top file is also `MODULE.bazel.lock` (38).

#### What was not measured

- Agent tokens or dollars per resolution.
- CI duration per rerun. Time in the merge queue itself is short: a median of 4.5 minutes in November.
- Why the ladder fell through on a given attempt. The reasons are logged, not stored on the row.
- Where PR lifetime goes between open and merge request. GitHub holds no approvals for this repository (0 approving reviews on 3,241 PRs since May), so the wait is only visible in engine state.

## Ideas

Each idea lists the measured source it addresses, expected impact, cost, risk and what it would take. Impact figures are shares of the 1,233 conflict states unless stated. They overlap and must not be added together.

### Source reorganisation

#### S1. Evaluating external-only Cargo lockfiles

**Verdict:** retain the current configuration. The [executed four-pair trial](#executed-lockfile-trial) below found two unresolved lockfile conflicts under each configuration and no consistent repin speedup. The combination removes the crate extension's record from `MODULE.bazel.lock`, but the JSON still contains a shared checksum and workspace dependency maps. Stripping by itself leaves manifest conflicts. The following source analysis explains the configurations tested; it is not an adoption recommendation.

The [BazelCon slides](https://static.sched.com/hosted_files/bazelcon2025/54/Hofbauer%20Zipline%20CargoSplicing.pdf), pages 5–7, propose feeding `Cargo.bazel.lock`, containing only external dependencies, to `from_cargo`. [rules_rust #3699](https://github.com/bazelbuild/rules_rust/pull/3699), merged November 10, 2025, replaces the separate stripping hook with `strip_internal_dependencies_from_cargo_lockfile`. Mono already pins 0.70.0, which contains the flag; no rules_rust upgrade is needed. This section evaluates that pinned implementation, not a newer release.

**What the recorded conflicts actually contain.** [measured] The earlier figure of 254 counts conflict **states with Cargo input hunks**, not individual hunks. Matching the earlier full-hunk replay to the final corpus reproduces that subset:

| Cargo input kinds in a state's conflict text     | Original subset | Final corpus |
| ------------------------------------------------ | --------------: | -----------: |
| `Cargo.lock` and one or more `Cargo.toml` inputs |             213 |          214 |
| `Cargo.lock`, with no manifest input             |              34 |           34 |
| Manifest inputs, with no `Cargo.lock` input      |               7 |            7 |
| Total with either Cargo input kind               |             254 |          255 |

The extra state is [mono#3544](https://github.com/spinyfin/mono/pull/3544), present in the final replay but absent from the earlier full-hunk replay. The other five of the 260 `MODULE.bazel.lock` states have neither kind of `FILE:` entry. A state can contain additional conflicts, so the categories above do not imply that all its hunks disappear.

Re-reading all 260 stored merge-result blobs yields **560 hunks**: 206 contain both kinds of changed input, 42 contain only the Cargo lockfile kind, 121 only the manifest kind, seven other `FILE:` inputs (`Package.resolved`), and 184 no `FILE:` input. There are **248 hunks containing `Cargo.lock` inputs and 327 containing manifest inputs**, with overlap. The root `Cargo.toml` occurs in 206 hunks and `engine/core/Cargo.toml` in 62. Embedded `defs.bzl` output also occurs in 171 of the 255 Cargo-input states. These are counts of changed input entries within conflict markers, not a counterfactual replay under the proposed configuration.

| Conflicting content                                                    | Effect of stripping alone                                                                                                                                                                                              |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `FILE:@@//Cargo.lock <sha>`                                            | Replaced by `FILE:@@//Cargo.bazel.lock <sha>`. First-party-only package changes no longer alter that hash **if the resolved external graph stays identical**. External version, source or dependency changes still do. |
| `FILE:@@//Cargo.toml <sha>` and `FILE:@@//…/Cargo.toml <sha>`          | Remain, including workspace-member additions and edits during crate extraction.                                                                                                                                        |
| Generated dependency maps, extension digests, other extensions' inputs | No general removal. Stripping does not change which manifests feed generation or disable other extensions.                                                                                                             |

Only **20 states** have `MODULE.bazel.lock` conflict text consisting solely of the Cargo lockfile input; in **17** it is also the only conflicted file. [estimate] Under the optimistic assumption that each of those edits leaves the external graph unchanged, 17/1,233 (1.4%) is the directly identifiable whole-state opportunity for stripping alone. This is not a measured elimination rate or a universal upper bound: regenerating may reshape hunks, and concurrent third-party changes still conflict. In particular, the 16% lockfile-only figure must not be presented as the flag's expected saving.

**Why manifests remain inputs.** [code] In 0.70.0, [`extensions.bzl:1114–1123`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/extensions.bzl#L1114-L1123) watches the configured Cargo lockfile and manifests unconditionally. For mono's root workspace manifest, [`splice.rs:185–200`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/src/cli/splice.rs#L185-L200) also collects workspace package manifest paths; [`extensions.bzl:672–676`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/extensions.bzl#L672-L676) watches those paths. The generator returns more tracked paths at [`generate.rs:273–305`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/src/cli/generate.rs#L273-L305), consumed at [`extensions.bzl:700–702`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/extensions.bzl#L700-L702). None of those branches tests the stripping flag.

The flag acts later, at [`generate.rs:224–255`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/src/cli/generate.rs#L224-L255): after writing the JSON context, it filters Cargo lockfile packages to those with `source.is_some()` and writes the result back. Thus “internal” means source-less/path packages, not an ownership classification. Leave `skip_cargo_lockfile_overwrite` false: setting it true skips the write containing the filter. There is no need for the talk's pre-commit hook. The normal `Cargo.lock` remains separate and is not kept synchronized by writes to `Cargo.bazel.lock`.

**Alternatives and combinations.** [code] [estimate] Costs below are engineering estimates from the source analysis. The executed trial below measures the two stripping configurations; the other alternatives remain untested. The actual API is `crate.from_specs` (plural), called “from_spec” in the talk.

| Configuration                                                                  | Lines removed from `MODULE.bazel.lock`                                                                                                                    | Remaining/new conflict point and mechanical resolution                                                                                                                                                                        | Effect on 126-manifest extraction trend; cost                                                                                                                                                                                                               |
| ------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `from_cargo` + stripped `Cargo.bazel.lock`, no JSON `lockfile`                 | Original `Cargo.lock` input becomes the external-only lockfile input; first-party-only hash churn shrinks. Manifest inputs and generated output remain.   | New `Cargo.bazel.lock`, sorted Cargo package records; merge disjoint packages or regenerate from merged manifests and reviewed external pins. Third-party conflicts still require version intent.                             | Removes first-party lockfile churn, not manifest or generated-map churn; small configuration change, medium integration cost for two lockfiles and their consumers.                                                                                         |
| `from_cargo` + JSON `lockfile`, existing `Cargo.lock`                          | Entire crate extension record, including both input kinds and embedded generated output. Other extensions and registry hashes remain.                     | New `cargo-bazel-lock.json`; one checksum plus structured crate/workspace maps. Maps can merge, but the checksum cannot be unioned: regenerate from merged inputs. Existing `Cargo.lock` still changes for first-party edits. | Extraction still changes the JSON context/checksum; moves the hotspot rather than removing its cause. Medium cost including deterministic regeneration support.                                                                                             |
| `from_cargo` + JSON `lockfile` + stripped `Cargo.bazel.lock`                   | Same whole-record removal as the previous row; stripping adds no further removal there.                                                                   | Both new files. Stripping stabilizes external Cargo pins for first-party-only changes; JSON checksum/maps still need regeneration.                                                                                            | Preserves manifest-derived helpers and removes first-party changes from one lockfile, while JSON still tracks workspace growth. Medium cost; tested below.                                                                                                  |
| External `crate.spec` entries + `from_specs`, separate Cargo lockfile, no JSON | Both first-party Cargo input kinds disappear because neither is read; the new external Cargo lockfile input and extension/generated-output record remain. | Specs in `MODULE.bazel`, external Cargo lockfile, and `MODULE.bazel.lock` on third-party changes. Merge specs first, then resolve/regenerate pins and extension output.                                                       | Pure first-party extraction no longer changes dependency inputs, assuming specs are external and independently maintained. Large migration cost: duplicate Cargo/Bazel dependency declarations, feature/platform parity and replace manifest-based helpers. |
| External specs + `from_specs` + both lockfiles                                 | Entire crate extension record.                                                                                                                            | Specs plus external Cargo lockfile and JSON checksum/maps, now driven by third-party specs rather than the first-party workspace. Regenerate after merging specs/pins.                                                        | Strongest separation from crate extraction; same large migration cost, plus JSON maintenance. Stripping is largely redundant without source-less packages; adding path specs restores tracking of those paths.                                              |

The whole-record removal is source-backed: [`extensions.bzl:1141–1151,1186–1190`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/extensions.bzl#L1141-L1190) requires **both** `lockfile` and `cargo_lockfile` for every configuration using the extension before returning `reproducible = True`; [Bazel then omits that extension's evaluation from the committed lockfile](https://bazel.build/external/lockfile#module-extensions). Manifests remain watched for invalidation even though their hashes leave this file. Adding only a JSON lockfile to `from_specs` is insufficient if its Cargo lockfile is absent.

The replacement checksum is also source-backed, not just speculation: [`lockfile.rs:106–151`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/src/lockfile.rs#L106-L151) hashes the rendered context, configuration, splicing metadata and tool versions; [`splicing.rs:103–151`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/src/splicing.rs#L103-L151) reads and parses the supplied manifests. Changes to the root member list therefore remain checksum inputs. Stripping happens after JSON generation and cannot remove the first-party workspace mappings from that context. The measured `Cargo.lock` yield of 0.07 versus 0.99 for `MODULE.bazel.lock` supports trying package-oriented data, but does **not** measure the new JSON's merge rate.

An explicit JSON lockfile also changes the update workflow: [`determine_repin`](https://github.com/bazelbuild/rules_rust/blob/0.70.0/crate_universe/private/generate_utils.bzl#L379-L435) fails on a stale checksum unless repinning is explicitly requested. Crate extraction may therefore require a repin even when external versions stay fixed. Include that developer cost in the comparison; reproducibility does not make first-party manifest changes free.

The earlier alternative of refreshing `MODULE.bazel.lock` only on `main` creates no new file but retains a shared generated file, a stale interval and a post-merge writer. It needs CI ownership and offers no source-backed advantage over the reproducible-extension trial. Keep other extensions' lockfile records committed.

**Configuration tested in the trial.** Retain the existing annotations and `use_extension`; replace only the existing `from_cargo` call with this, preserving all eight supported platforms:

```starlark
crate.from_cargo(
    name = "mono_crates",
    cargo_lockfile = "//:Cargo.bazel.lock",
    lockfile = "//:cargo-bazel-lock.json",
    manifests = ["//:Cargo.toml"],
    strip_internal_dependencies_from_cargo_lockfile = True,
    supported_platform_triples = [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "wasm32-unknown-unknown",
        "wasm32-wasip1",
        "wasm32-wasip2",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "x86_64-unknown-nixos-gnu",
    ],
)
use_repo(crate, "mono_crates")
```

[code] This fits mono's existing `all_crate_deps` consumers (for example `tools/boss/engine/core/BUILD.bazel`). A specs-only conversion would lose their per-workspace-package dependency mapping. Export the two new lockfiles from the root BUILD package with minimal visibility, seed the Cargo-format file from the reviewed pins, initialize the JSON via rules_rust's repin path, and review any version changes. Do not rename the normal `Cargo.lock` away: the release script, checkleft version metadata and `protocol-spoke:toolchain_coherence_test` read it. Adoption must either verify external-pin parity or deliberately redirect Bazel-specific readers to the Bazel lockfile so they check what Bazel actually builds.

Regeneration is mechanical **after dependency intent is merged**, not permission to discard one side's version changes. Merge manifests/specs and external pins first, regenerate JSON with the pinned Bazel/rules_rust toolchain, and require a second evaluation to leave all lockfiles unchanged. The engine currently recognizes `Cargo.lock` and `MODULE.bazel.lock` by basename (`deterministic-resolvers/src/resolvers/generated_lock.rs:33–38`), not these two proposed names; extend its existing resolver before claiming automatic resolution. Do not deploy the configuration merely to move conflicts into a file the ladder cannot handle.

**Decision criterion:** compare the combination against stripping-only and the current configuration on the same replayed crate-extraction and third-party-update pairs. Count conflicts across **all** lockfiles, time repinning with normal caches, check dependency/helper parity, and verify repeat evaluations are stable. Prefer the combination only if it reduces total unresolved conflicts or makes regeneration reliably cheaper. The [executed trial](#executed-lockfile-trial) did not meet either criterion, so this change records the results and retains the existing build configuration and resolver.

**S2. Remove the shared append points in the schema migration chain.**

- **Addresses:** 99 states caused by the chain alone (8.0%), 128 that touch it. Yield 0.84 since named migrations landed.
- **Options:** one file per migration with registration derived at build time or through a distributed registry such as `inventory` or `linkme`; or, minimally, keep the registry array but order it by migration name instead of inserting at the top, and enforce the order with a test, as `FrontendRequest` does. The runner already sorts entries by id regardless of source order ([database migrations](../../tools/boss/docs/database-migrations.md)), so the array's order carries no meaning.
- **Impact:** [estimate] most of the 8%. A name-ordered array spreads insertions across the file; per-file registration removes them.
- **Cost:** small for sorting, medium for per-file registration.
- **Risk:** the test's list is different. It is deliberately independent of the registry and applies its calls in order, so a migration that builds on an earlier one must stay after it. Either pair each call with its id and sort when the test runs, or leave the list as it is and let a keep-both rule resolve it (E3). A distributed registry adds a linker-level mechanism to a crate that currently has none.
- **Takes:** one PR, then compare yield over the next 30 touches.

**S3. Break up the five giant wrapped import lists.**

- **Addresses:** 180 hunks; 59 states caused by import lists alone (4.8%), 153 that include one.
- **Options:** one `use` statement per name in those five lists, which stable rustfmt should keep separate and sort (not tested here); or a glob re-export where the list is a pure re-export. The toolchain is stable 1.99.0, so rustfmt's unstable `imports_granularity` and `imports_layout` options are unavailable.
- **Impact:** [estimate] most of the 4.8%, and it shrinks another 94 states.
- **Cost:** small, mechanical, five files.
- **Risk:** low. Adjacent names can still collide. A glob re-export widens what the crate exports.
- **Takes:** one PR.

**S4. Apply the sorted-list pattern to remaining count and append sites.**

- **Addresses:** the pattern that cut `metrics_init.rs` yield from 1.00 to 0.36.
- **Impact:** [estimate] small per site. Worth doing when the hotspot report names a site, not as a sweep.
- **Risk:** sorting clusters on common prefixes, as the `Get…` variants show.

**S5. Split the hub types that every subsystem extends.**

- **Addresses:** `ServerState`, its constructor and `handle_frontend_connection` (`app.rs`, 83 states by file name), `ExecutionCoordinator`, `run_execution`.
- **Impact:** [estimate] a few percent each; these are genuine concurrent edits, so splitting moves some of them apart and leaves the rest.
- **Cost:** large. The [crate decomposition](engine-lib-crate-decomposition.md) is already moving this code for build-time reasons, and the moves themselves caused almost no conflicts.
- **Takes:** nothing new; fold conflict yield into the existing decomposition's choice of seams.

**S6. Keep PRs small.**

- **Addresses:** the size gradient in Finding 2. Median files per PR went from 3 to 19; a 21 to 40 file PR conflicts 46% of the time against 9% for 3 to 5 files.
- **Impact:** [estimate] large if achieved, but it trades against task granularity.
- **Takes:** treat the existing 30-file limit as a ceiling to stay well under, and prefer capability-sized tasks when planning.

**S7. `.gitattributes` merge drivers: not viable here.** GitHub does not apply custom or `union` merge drivers when computing PR mergeability (community reports and an [open request](https://github.com/orgs/community/discussions/9288); I found no official statement) and jj does not read `.gitattributes` ([jj Git compatibility](https://docs.rs/crate/jj-cli/latest/source/docs/git-compatibility.md)). Detection uses the first and resolution the second, so a driver would change neither. Bazel documents a jq-based merge driver for `MODULE.bazel.lock`, but for conflicts outside two specific sections its guidance is to regenerate with `bazel mod deps` ([Bazel lockfile](https://bazel.build/external/lockfile)), which is what the engine's resolver already does.

**S8. Other generated files.** `Cargo.lock` is not a problem (yield 0.07). No other generated file appears in the top 50.

### Engine

**E1. Fix the diagnosis base.**

- **Addresses:** the empty diagnosis on 88% of attempts.
- **Change:** merge the PR head against the fetched tip of the base branch, not `base_sha_at_trigger`.
- **Impact:** no conflict is avoided directly. The resolver's prompt gains a correct file list, `conflict_class` and the hotspot report become usable, and every later idea that routes on conflict shape becomes possible.
- **Cost:** small.
- **Risk:** low. `main` can move between detection and diagnosis; record the tip used.
- **Takes:** one PR, verified by comparing stored diagnoses with a replay for a week.

**E2. Make the deterministic tier land, and record why it does not.**

- **Addresses:** 198 lockfile-only states. Since default-on, 6 of 27 resolved ones avoided an agent.
- **Change:** store the ladder's fall-through reason and tier outcome on the ledger row and as counters, then fix the leading causes. The design postmortem already lists the missing counter as a gap.
- **Impact:** [estimate] up to 16% of states move from a 10 to 15 minute agent run to about 5 minutes with no model.
- **Cost:** small for the recording; unknown for the fixes until the reasons are visible.
- **Risk:** low. The deletion tripwire already guards these tiers.

**E3. A keep-both resolver for same-spot insertions.**

- **Addresses:** 360 states (29.2%) made only of same-spot insertions and import-list edits, of which 167 involve only whole items or import entries.
- **Change:** extend the existing union resolver beyond `mod.rs`/`lib.rs` to any file where every hunk is two self-contained insertions, apply both, reformat, and let the existing build and test gate decide. Wire the declarative recipes, which are built but not loaded, for per-path rules such as the migration lists.
- **Impact:** [estimate] 13% with high confidence (items and imports), up to 29% if statement and list insertions pass the gate.
- **Cost:** medium.
- **Risk:** keeping both can be wrong when order matters or when both sides added the same thing. The gate catches compile and test failures, not silent duplication. Start with items and imports.
- **Takes:** an offline replay on the corpus from this investigation to measure the pass rate before shipping.

**E4. Resolve just in time.**

- **Addresses:** the 427 conflict states on merged PRs that were followed by another conflict before the merge, and the 292 on PRs that have not merged: 719 of 1,233 (58%).
- **Change:** when a PR conflicts while it is still waiting for review or other work, record it and do nothing. Resolve when the PR is otherwise ready to merge, or when a human asks.
- **Impact:** [estimate] 58% is an upper bound on resolutions avoided. The real figure depends on how early PRs are marked ready, which GitHub does not show.
- **Cost:** medium. A readiness signal the engine already has, plus a UI state for "conflicted, will resolve at merge".
- **Risk:** each merge of a conflicted PR waits for one resolution and a CI run after approval (the resolution alone is a median of 12 minutes overall and 26 in October). A reviewer sees a stale diff against `main`. Deferred conflicts can grow, although Finding 5 shows resolution time barely depends on size.
- **Takes:** an A/B split by PR number comparing resolutions per merged PR and time from merge request to merge.

**E5. A policy for stale PRs.**

- **Addresses:** 44 open PRs holding 235 conflict states (19%), and 68 open PRs conflicting today.
- **Change:** after a set number of resolutions or days, stop auto-resolving and ask for a decision: merge now, close, or rewrite on current `main` (see N3).
- **Impact:** [estimate] up to 19% of resolution work, and a shorter open-PR list, which lowers exposure for every other PR.
- **Cost:** small.
- **Risk:** adds questions for the repo owner. Batch them.

**E6. Cut the fixed overhead of an agent resolution.**

- **Addresses:** the flat 15 minutes in Finding 5.
- **Change:** reuse the workspace the ladder already rebased instead of leasing and rebasing again (deferred in the earlier design), and scope the pre-push gate to targets affected by the conflicted files.
- **Impact:** [speculation] several minutes per resolution; not measurable without per-phase timings.
- **Takes:** per-phase timing on the ledger row first.

**E7. Overlap-aware scheduling: keep it narrow.**

- **Evidence:** [measured] of 31,663 pairs where one PR merged while another was open, 5,044 shared a file, and at most 1,164 of those (23%) were involved in a conflict. Holding back work on any shared file would delay about three harmless pairs for each conflict avoided.
- **Verdict:** serialising dispatch on file overlap is not supported by the data, which agrees with the earlier design's rejection. The files where overlap nearly guarantees a conflict (yield above 0.6) contain the shared inputs targeted by S1 and S2; S1's subsequent trial did not demonstrate a net reduction.

**E8. Rebasing earlier or more often: no.** [measured] 73% of conflicts come from a single merge and the branch is a median of 6 commits behind. An earlier rebase finds the same conflict sooner and adds a CI run. The default-off speculative pass predicts conflicts but cannot prevent them.

**E9. Merge queue: already in place.** [measured] mono lands through GitHub's queue with a median of 4.5 minutes in the queue. 116 of about 3,400 queue entries were ejected for a merge conflict. The queue is not where the time or the conflicts are.

**E10. Shorter PR lifetimes.**

- **Addresses:** the strongest gradient in the data. Under an hour open, 5% conflict; over three days, 66%. In May, at 25 merges a day and a 6-minute median lifetime, 7% of PRs conflicted.
- **Impact:** [estimate] conflicts scale roughly linearly with lifetime at fixed throughput.
- **Takes:** first measure where the wait is, from engine state. This investigation could not see it.

**E11. Record mid-task conflicts from the pre-push rebase.** Have `cube workspace local-rebase` report conflicts to the ledger itself, so the count does not depend on a worker remembering a second command.

### Novel

**N1. Syntax-aware merge as a tier before an agent.**

- **What:** [Mergiraf](https://mergiraf.org/) merges syntax trees instead of lines and resolves insertions into containers whose order does not matter, such as imports, fields and methods. It supports Rust, Starlark, TOML, JSON and YAML but not Swift or Markdown ([languages](https://mergiraf.org/languages.html)). jj can call it with `jj resolve --tool mergiraf` ([usage](https://mergiraf.org/usage.html)).
- **Addresses:** [estimate] the 167 states of item and import insertions (13.5%), plus an unknown share of the 675 states where both sides changed the same lines but possibly different syntax elements. It does not resolve same-spot statements, and 75 states are Swift only.
- **Cost:** small to trial: a tool on the host and one more step in the ladder after the engine rebase. The host has jj 0.26.0, whose default configuration has no Mergiraf entry, so it needs either an explicit merge-tool entry or a newer jj.
- **Risk:** a merge that parses but is semantically wrong. The build and test gate and the deletion tripwire apply as for any other tier.
- **Takes:** the offline experiment below. It was not run here because it means installing a tool on the host.

**N2. Using jj's first-class conflicts differently.**

- **Squash before rebasing.** Every PR is squash-merged, so only the final tree matters. [measured] The saving is small: 1.18 conflicted commits per state today.
- **Carry the conflict instead of resolving it.** jj can keep a conflicted commit locally, which is what E4 needs: the engine can track the conflicted rebase without pushing and materialise a resolution only at merge time.
- **Reuse earlier resolutions.** 73% of repeat conflicts hit a file that already conflicted on the same PR. [speculation] jj has no built-in equivalent of git's `rerere` that I know of, and for lockfiles and list appends the deterministic tier makes reuse unnecessary.

**N3. Intent-level replay.**

- **What:** instead of merging text, run the PR's task again on current `main`, using the PR description and diff as the specification. The engine already offers this as "rewrite onto the new base" when a resolver reports an architectural mismatch.
- **Addresses:** stale and repeatedly conflicted PRs (E5), and the rare large conflict: 13 agent-resolved states had 10 or more hunks, with a median of 48 minutes and a 90th pct over 19 hours.
- **Impact:** [speculation] small in count, large per case. One PR has conflicted 34 times and is still open.
- **Risk:** the rewritten PR is new code and needs a full review; anything a reviewer already approved is invalidated.
- **Takes:** offering it as one of the E5 choices and tracking how often it is picked and how the result fares.

**N4. Land overlapping PRs as a train.** [measured] Between 2026-11-09 and 2026-11-11 three sibling PRs (#3466, #3468, #3473) each hit a lockfile-only conflict four to seven times as the others and their neighbours merged. [speculation] Resolving each against the next in a fixed order would have cost about one resolution per PR. The stacked-PR offer that detects such pairs exists but is default-off and cannot yet act, and its value shrinks if S1 and E4 land.

## Ranked recommendation

| #   | Change                                                           | Addresses (share of conflict states)                         | Cost   | Confidence                                   |
| --- | ---------------------------------------------------------------- | ------------------------------------------------------------ | ------ | -------------------------------------------- |
| 1   | E1: diagnose against current `main`                              | Enables the rest; 88% of diagnoses empty                     | Small  | High: reproduced on every row                |
| 2   | E2: record ladder outcomes, make lockfile-only retire            | 16% (30% in November)                                        | Small  | High on the gap, unknown on the cause        |
| 3   | S2: schema chain append points                                   | 8% sole cause                                                | Small  | High: the metrics precedent cut yield by 2/3 |
| 4   | S3: five import lists                                            | 5% sole cause, 12% touched                                   | Small  | High                                         |
| 5   | S1: evaluate reproducible `from_cargo` + stripped Cargo lockfile | Cargo input hunks in 255 states; JSON conflicts replace some | Medium | Four-pair trial: no demonstrated net benefit |
| 6   | E5: stale-PR decision                                            | Up to 19% of resolution work                                 | Small  | Medium: needs a decision per PR              |
| 7   | E4: just-in-time resolution                                      | Up to 58% of resolutions (upper bound)                       | Medium | Medium: depends on the readiness signal      |
| 8   | E3: keep-both resolver                                           | 13% to 29%                                                   | Medium | Medium: needs the replay experiment          |
| 9   | N1: syntax-aware merge tier                                      | 13% plus an unmeasured part of the hard 55%                  | Small  | Unknown until tested                         |
| 10  | E10 and S6: shorter-lived, smaller PRs                           | The underlying rate                                          | Varies | High on the effect, unknown on the means     |

Items 1 to 4 are independent and small. Together they address about 30% of today's conflict states (the lockfile, schema chain and import-list sole causes) and add the instrumentation needed to steer the rest. The cost of item 2 is small for the recording and unknown for the fixes until the reasons are visible. Item 5 has now been tried on four pairs without meeting its adoption criterion; keep the current configuration. Any later adoption must integrate the deterministic tier for both proposed files and demonstrate a net benefit across all lockfiles. Stripping alone is a small adjunct; `from_specs` is a larger dependency-model migration and is not justified by these measurements alone.

Not recommended: `.gitattributes` merge drivers (S7), more frequent rebasing (E8), general file-overlap serialisation (E7), and a change of merge queue for conflict reasons (E9).

## Experiments that would confirm the top ideas

1. **Diagnosis fix.** After E1, compare each stored diagnosis with a replay against `main` at detection. Expect the file lists to match on at least 95% of rows and `unknown` to fall below 5% of classes.
2. **Ladder outcomes.** After recording the fall-through reason, tabulate reasons for lockfile-only conflicts over two weeks. Success is at least 90% of lockfile-only conflicts retiring without an agent.
3. **Offline merge-tool replay.** Rebuild the corpus (see the appendix), extract base, ours and theirs for the 1,860 content-conflicted file rows, and run the candidate resolver on each: first a keep-both union for E3, then Mergiraf for N1. Report the share of files and of whole conflict states resolved, then build and test a sample of 50 resolved states. This needs no production change.
4. **Schema chain and import lists.** After S2 and S3, track conflict states per `main` commit touching those files. Baselines: 0.84 for the schema chain; 180 import-list hunks in six months. Expect a drop of at least half over the next 30 touches, as the metrics change showed.
5. **Just-in-time resolution.** Split PRs by number parity for two weeks. Compare resolutions per merged PR, time from merge request to merge, and how often a deferred conflict needed a human.
6. **Lockfile spike — four-pair trial completed.** The [results below](#executed-lockfile-trial) compare the current configuration, stripping alone, and S1's reproducible-plus-stripping configuration on identical crate-extraction and third-party-update pairs. They report changed files, unresolved conflicts across all lockfiles, repin duration with normal caches, dependency/helper parity, and repeat-evaluation stability. The baseline is 255 states with Cargo input conflicts, not 255 states known to disappear. The trial did not justify adoption; the existing resolver and configuration remain unchanged.
7. **Where PR lifetime goes.** From engine state, split lifetime into waiting for CI, automated review, human review and merge. This decides whether E10 is tractable.

## Follow-up code changes

These are for separate PRs; none was made here.

- Pass the base branch tip, not `base_sha_at_trigger`, to the pre-spawn conflict diagnosis (`tools/boss/engine/dispatch/src/coordinator/run.rs`, `collect_conflict_diagnosis_for_attempt`).
- Store the ladder's outcome and fall-through reason on `conflict_resolutions` and add tier-outcome counters.
- Order the `schema_migration_registry.rs` array by migration name, with a test enforcing the order, and make the migration list in the `schema_init.rs` test order-independent in source; or move to one file per migration.
- Convert the five large wrapped `use` lists to one `use` per line or a glob re-export.
- Have `cube workspace local-rebase` report conflicts to the ledger.
- Exclude or separately label rows without a trustworthy diagnosis in `boss engine conflicts hotspots`, until the diagnosis fix has replaced them.

## Appendix: rebuilding the corpus

1. Export the ledger: `boss engine conflicts list --json --product <slug> --limit 100000`.
2. Fetch PR metadata with one paginated GraphQL query over `pullRequests`, including `timelineItems` of type `HEAD_REF_FORCE_PUSHED_EVENT` (before and after commits) and the merge-queue events.
3. Make a bare clone outside any workspace, fetch `+refs/pull/*/head:refs/pull/*/head`, then fetch every ledger `head_sha_before` and every force-push "before" commit by SHA.
4. For each in-review row, take `main` at the row's `created_at` with `git rev-list -1 --first-parent --before=<ts> main` and run `git -c merge.conflictStyle=diff3 merge-tree --write-tree -z <main> <head>`. Exit status 1 means conflict; the output lists the conflicted paths and stages, and the written tree holds each file with conflict markers.
5. Classify hunks from the three marker sections: base empty means both sides inserted; one side matching the base as a subsequence means insert against edit; otherwise both changed. A hunk counts as self-contained when each side's inserted text has balanced brackets.
6. For the onset, walk the first-parent commits between the merge base and that `main` commit that touch a conflicted file, and take the first one against which the head conflicts.

The scripts were run from a scratch directory and are not part of this change.

**S1 recount method.** Select the earliest conflicted row for each `(pr, head)` from `replay_L.jsonl`, yielding 1,233 states; select its 260 `MODULE.bazel.lock` rows. Read each stored result tree's lockfile blob and parse every diff3 marker block without truncating long generated-output lines. Within each block, parse `FILE:@@//<path> <digest>` entries on ours/base/theirs and count a path when its value or presence differs. Group paths into root `Cargo.lock`, any `Cargo.toml`, and other inputs; count both hunks and distinct states, retaining overlaps. Separately compare non-input lines and count embedded `defs.bzl` entries. The earlier `replay2.json` contains 259 matching `(main, head)` pairs, including the 254-state Cargo-input subset; the final corpus adds the state on #3544. Reading retained merge-result objects suffices for these counts; no merge or resolution was rerun. That recount did not measure a hypothetical external graph or its mergeability; the subsequent S1 trial below does so for four pairs.

## Executed lockfile trial

### Method and inputs

[measured] The November 11–12, 2026 trial uses four recorded review-conflict pairs: crate extraction in [#3430](https://github.com/spinyfin/mono/pull/3430) and [#3438](https://github.com/spinyfin/mono/pull/3438), and concurrent branches crossing the external dependency changes in [#3091](https://github.com/spinyfin/mono/pull/3091) and [#3262](https://github.com/spinyfin/mono/pull/3262). The latter pairs use the recorded heads of [#3050](https://github.com/spinyfin/mono/pull/3050) and [#3257](https://github.com/spinyfin/mono/pull/3257). This is a four-pair sample, not a rerun of the entire 260-state corpus. Each configuration uses the same merge base, PR head and main revision:

| Pair          | Merge base                                 | PR head                                    | Main at detection                          |
| ------------- | ------------------------------------------ | ------------------------------------------ | ------------------------------------------ |
| #3430         | `8d559994b81ef9b46349e1ecc3ed24f7a82e00f1` | `c60e927b968814b72dfcbfad5279cad08b7e9674` | `fd5f0e8459d498858545821d5e2b0ecadd354710` |
| #3438         | `99287d1e8cfcd7b139ce669bf330f4f9ac3e086e` | `950032f5c20f0ad9c5163b7239fad5d31ec37c01` | `148dbb76f75ec377ea5fd43d5172590fa69591ad` |
| #3050 / #3091 | `ab6355744cfaa5c31cd7c7999963273aed094473` | `f434dcf24e36476c33c8ae506d319f4f8cd307e6` | `a06c56a0435f7065274d88e903df9dfea83ad745` |
| #3257 / #3262 | `bc27dd4ee14922238a1f1e77662dab40370bba8a` | `4a4816402281e853b62531dcc56e7e5c3e846a95` | `8b04af4d1bb6d3e900760ad938d5b760d1b04600` |

The heads come from the conflict ledger; main is the latest ancestor by committer timestamp at detection (Unix seconds 1793580455, 1793659656, 1791333848 and 1792720436 respectively). The merge base is the common ancestor of that pair. These are fixed replay inputs, not a claim to recover an unrecorded exact GitHub check timestamp. Where a historical head was unavailable in the scratch clone, its complete `jj diff --git` from the merge base was applied to that base.

All 12 revision endpoints were evaluated under each of these configurations, for 36 evaluations:

| Configuration               | `cargo_lockfile`      | `lockfile`                 | Stripping | Files changed relative to the reviewed endpoint                             |
| --------------------------- | --------------------- | -------------------------- | --------- | --------------------------------------------------------------------------- |
| A: current                  | `//:Cargo.lock`       | absent                     | false     | None on ordinary evaluation                                                 |
| B: stripping only           | `//:Cargo.bazel.lock` | absent                     | true      | `MODULE.bazel`, `BUILD.bazel`, `MODULE.bazel.lock`; adds `Cargo.bazel.lock` |
| C: reproducible + stripping | `//:Cargo.bazel.lock` | `//:cargo-bazel-lock.json` | true      | B's files; also adds `cargo-bazel-lock.json`                                |

The historical rules_rust 0.70.0 pin, annotations and all eight supported platform triples were retained. `skip_cargo_lockfile_overwrite` remained false. B and C seed the Cargo-format file from that endpoint's reviewed `Cargo.lock`; root BUILD exports use `visibility = ["//:__pkg__"]`. C starts with an empty JSON file and initializes it using `CARGO_BAZEL_REPIN=true bazel mod deps --lockfile_mode=update`. A and B initially use ordinary `bazel mod deps --lockfile_mode=update`; B automatically invokes the generator because its configuration changed. C then evaluates with the repin variable unset. Every endpoint has a further ordinary evaluation and `bazel query @mono_crates//:all` to materialize the generated helpers. All commands use normal Bazel caches.

For each pair and configuration, the four lockfile paths (`Cargo.lock`, `MODULE.bazel.lock`, `Cargo.bazel.lock`, `cargo-bazel-lock.json`) are copied into a separate jj repository. Base is committed first; head and main are separate children of base; `jj new <head> <main>` and `jj resolve --list` measure unresolved files. No conflicted file is replaced with one side, and no dependency choice is made during the merge. Only `Cargo.lock` and `MODULE.bazel.lock` changed among existing lockfiles in these reviewed pairs. This measures jj 0.26.0's merge result, whereas the original corpus used Git merge-tree. Replaying the original lockfiles with jj leaves a lockfile conflict in two pairs; the other two serve as controls.

### Conflict results

| Pair                               | A: unresolved lockfiles  | B: unresolved lockfiles  | C: unresolved lockfiles  |
| ---------------------------------- | ------------------------ | ------------------------ | ------------------------ |
| #3430 extraction                   | `MODULE.bazel.lock`      | `MODULE.bazel.lock`      | `cargo-bazel-lock.json`  |
| #3438 extraction                   | None                     | None                     | None                     |
| #3050 / #3091 dependency update    | `MODULE.bazel.lock`      | `MODULE.bazel.lock`      | `cargo-bazel-lock.json`  |
| #3257 / #3262 dependency additions | None                     | None                     | None                     |
| **Total**                          | **2 files in 2/4 pairs** | **2 files in 2/4 pairs** | **2 files in 2/4 pairs** |

C removes the crate extension's entire committed MODULE record, but both unresolved conflicts move into its JSON context. In #3430 both sides change the JSON checksum and crate map; the extraction also changes workspace members. In #3050 both sides change the checksum and crate map, and main changes direct dependencies. B retains manifest hash conflicts. Neither `Cargo.lock` nor `Cargo.bazel.lock` conflicts in this sample; the total conflict count does not fall.

### Dependency parity and stability

All 36 endpoints preserve the complete external package records from their reviewed `Cargo.lock`, including source, checksum and dependency arrays. Package counts for base/head/main are 541/541/541 for each extraction pair, 491/491/492 for #3050, and 492/492/533 for #3257. This retains the dependency updates on main as well as the historical head's pins. B and C contain no source-less packages in `Cargo.bazel.lock`; the ordinary `Cargo.lock` stays byte-identical.

At every endpoint, B and C's generated `defs.bzl` and hub `BUILD.bazel` are byte-identical to A's. Comparing the whole helper file covers `all_crate_deps`, its workspace-package maps and platform conditions, rather than checking only one consumer. This is dependency/helper parity, not a claim to have built every historical platform.

| Configuration | Byte-stable second ordinary evaluation              | Second ordinary evaluation wall time |
| ------------- | --------------------------------------------------- | ------------------------------------ |
| A             | 12/12                                               | 0.077–0.901 s                        |
| B             | 0/12 immediately after initial stripping            | 22.252–915.598 s                     |
| C             | 12/12, after repin and an unset-variable evaluation | 0.125–0.946 s                        |

B's initial generation rewrites its watched Cargo input by stripping workspace packages. The next evaluation updates the recorded input digest, so two evaluations are insufficient for byte stability from the initial full-lockfile seed. A third evaluation of #3430's base completed in 0.154 s and changed no files. C's stability measurement begins after removing the repin environment variable; it is not evidence that an explicit repin itself is byte-stable. A's short times are cached evaluations, not repin timings.

### Repin measurements

The initial runs exposed the generator's default 600-second subprocess timeout: an unchanged A configuration completed one explicit repin in 572.423 s, then failed its second at 604.789 s with `cargo-bazel splice: Timed out`; B's first #3430-base generation similarly failed at 605.248 s. Stripping happens after splicing, and the unchanged configuration reproduces the failure. A diagnostic retry with `CARGO_BAZEL_DEBUG=true CARGO_BAZEL_TIMEOUT=7200` allowed B to complete; the original failed observations are retained here. C's first #3430-base initialization took 993.236 s with that subprocess limit. No shell timeout or cache override was used, and no repository timeout/check setting was changed.

The remaining initial B automatic regenerations took 30.185–72.285 s, and the remaining C initializations took 33.737–107.129 s. Those are different workflows, run as caches warmed, so their medians are not a controlled speed comparison. A's original evaluations mostly reused its committed extension record.

For a comparable measurement, each historical PR head was restored with its already-initialized A, B or C files, then explicitly repinned twice. Every repin ran `CARGO_BAZEL_REPIN=true bazel mod deps --lockfile_mode=update`, followed by the same command with that variable unset. All configurations used the same diagnostic settings (`CARGO_BAZEL_DEBUG=true`, `CARGO_BAZEL_TIMEOUT=7200`), Bazel 9.1.0, one scratch checkout and normal caches on the same macOS host. Configuration order rotated A/B/C, B/C/A, C/A/B, A/B/C across the four heads. Complete external-pin equality was checked after each repin/evaluation pair.

All 24 explicit repins and their 24 subsequent ordinary evaluations exited 0. Wall times in seconds, with both repin observations shown:

| PR head                               | A: current       | B: stripping only | C: reproducible + stripping |
| ------------------------------------- | ---------------- | ----------------- | --------------------------- |
| #3430                                 | 18.040 / 37.305  | 22.716 / 20.853   | 18.761 / 18.968             |
| #3438                                 | 29.920 / 35.339  | 23.465 / 42.730   | 31.843 / 54.080             |
| #3050                                 | 19.003 / 17.039  | 27.748 / 30.356   | 26.823 / 31.963             |
| #3257                                 | 30.775 / 116.649 | 52.986 / 34.962   | 42.536 / 42.237             |
| **Median of eight repins**            | **30.348**       | **29.052**        | **31.903**                  |
| Repin range                           | 17.039–116.649   | 20.853–52.986     | 18.761–54.080               |
| Subsequent ordinary-evaluation median | 24.815           | 35.823            | 0.573                       |

C avoids another splice when the repin variable is removed, but its repin itself is not consistently faster. The extra A evaluation in this benchmark is induced by changing the repin environment: normal A regeneration can use ordinary `bazel mod deps` without that toggle. Its cached ordinary evaluations were already below one second. Thus the faster post-repin C evaluation is not a demonstrated reduction in the cost of the existing resolver's ordinary regeneration path. These are wall times on one host with network and cache warming effects, not a controlled CPU benchmark or a prediction of CI latency.

### Decision

**Do not adopt C on this evidence.** It leaves the same total unresolved lockfile conflicts as A and B, moves both observed conflicts to the JSON file, and does not make repinning reliably cheaper. It does preserve dependency/helper parity and produces a stable ordinary evaluation after repinning. Those properties establish feasibility, but do not satisfy the adoption criterion.

`MODULE.bazel`, root BUILD exports, production lockfiles and the generated-lock resolver remain unchanged. `Cargo.lock` continues serving the release script, checkleft metadata and toolchain coherence test. No new lockfile is introduced that the current resolver cannot regenerate, and neither side of an external version change is discarded. This conclusion applies to the four measured pairs; it does not estimate an elimination rate for the full historical corpus.
