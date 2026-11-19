# An intelligent merge queue for mono: baseline, prior art, and whether to build one

- **Date:** 2026-11-18
- **Kind:** investigation (read-only). No source, build, CI, GitHub, Buildkite or engine settings were changed.
- **Question:** should we build our own merge queue (Bazel-aware grouping, automatic resolution of trivial conflicts and CI failures, parallel lanes), and if so how?
- **Scope:** every pull request in `spinyfin/mono` from 2026-02-15 to 2026-11-18, with the merge queue period (from 2026-05-08) analysed in detail. `brianduff/flunge` is covered briefly as the one repo already on Trunk.
- **Data:** GitHub timelines for all 3,571 PRs, all 3,414 Buildkite merge-group builds, logs for 262 failed merge-group jobs, a throwaway clone used to diff 614 queue commits and replay 132 conflict ejections, the engine's CI-remediation ledger (1,196 rows) and run-cost records, and a `bazel query` of the test dependency graph. See [How this was measured](#how-this-was-measured).
- **Related:** [why merge conflicts are frequent](merge-conflict-sources-and-remedies.md), [moving mono's queue to Trunk](../../tools/boss/docs/investigations/mono-merge-queue-github-to-trunk-2026-10-14.md), [Trunk integration design](../../tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md), [conflict reduction and fast resolution design](../../tools/boss/docs/designs/merge-conflict-reduction-and-fast-resolution-for-parallel-tasks.md), [conflict resolution deferral](../../tools/boss/docs/conflict-resolution-deferral.md), [keep-both resolution](../../tools/boss/docs/conflict-keep-both-resolution.md), [incident 002](../../tools/boss/docs/postmortems/incident-002-merge-conflict-deletion-blessed-by-review.md).
- **Evidence labels:** **[measured]** computed from the data above. **[code]** read in this checkout at the cited path. **[web]** taken from the linked page on 2026-11-18 through an automated fetch. Four load-bearing pages (GitHub's queue documentation, Mergify's conflict resolution, Aviator's flaky test management and Uber's open-source repository) were fetched a second time and checked against the claims made here; the rest were not. **[estimate]** derived from measured numbers under a stated assumption. **[opinion]** judgement.

One PR in five that asks to merge is now thrown back out of the queue at least once, and those PRs account for 97% of all the time spent between "merge" and "landed". This document measures why, tests the thesis that a smarter queue would fix it, surveys what others have built, and recommends what to build and in what order.

## Verdict

**The thesis is half right. Build the half the data supports first, and put the other half behind measured triggers.**

**The queue's ordering and batching are not where changes are lost.** A PR that is not ejected lands a median of 2.8 minutes after the merge request. GitHub's queue has never ejected a PR because a different PR ahead of it failed (0 of 345 ejections). The queue is empty for 60% of enqueues and holds five or more entries for 1.6%. Only about 10% of PR pairs that share the queue are independent in the Bazel graph. Smarter batching and affected-target lanes would have removed no ejections in six months.

**The loss is ejection, and what happens after it.** 12% of merged PRs were ejected at least once (21% since October). They take a median of 48 minutes to land and a 90th percentile of 24 hours, against 2.8 and 6.4 minutes for the rest.

**The largest single cause is one the thesis does not name: flaky and infrastructure failures.** 47% of failed-check ejections ended with the PR merging later with a byte-identical patch (54% since September). A commit that has already passed the queue fails its next build on `main` 6% to 7% of the time. No queue we use retries before ejecting, and every ejection starts an agent run.

**The thesis is right about resolution.** 40% of merge-conflict ejections are a lockfile to regenerate or two insertions at the same spot. They take a median of 47 to 106 minutes to land again. Among failed-check ejections that needed a real change, a third were fixed by ten changed lines or fewer (a quarter since October).

**So the valuable thing to build is the ejection handler, not the scheduler.** Retry, deterministic resolution, a tightly bounded fix, and automatic requeue under the standing merge request would handle roughly 60% of today's ejections without an agent or a person, and cut their recovery from tens of minutes to about one CI cycle. Those ejections hold about 28% of the hours lost; the rest are in changes that need real work. All of it can run inside the engine against the queues we already use.

**Recommended sequence:**

1. Stop ejecting for flakes: re-enqueue without an agent, and fix the worst tests.
2. Finish the deterministic conflict tier and check for conflicts before enqueueing, building on the merge-intent work that already landed.
3. Add a bounded fix tier in shadow mode, and put a backend seam under the engine's merge path.
4. Build our own queue core only when a named trigger fires, as a separate crate behind that seam.
5. Add affected-target lanes only if a shadow measurement shows independence well above today's 10%.
6. Graduate to a standalone service only when PRs that Boss does not manage need it.

No hosted queue fits better: none retries Buildkite commit statuses the way we need, none can run `bazel mod deps`, and none can use the engine's merge intent. Details are in [Build, adopt or extend](#build-adopt-or-extend).

## How this was measured

**Sources.**

- **GitHub.** One GraphQL pass over all 3,571 PRs (3,343 merged, 135 open, 93 closed) for dates, size, and the `AddedToMergeQueueEvent` and `RemovedFromMergeQueueEvent` timeline items with their `reason` and `beforeCommit`. That gives 3,598 queue entries and 3,594 removals.
- **Buildkite.** Every build of the `mono` pipeline on a `gh-readonly-queue/main/*` branch (3,414 builds from 2026-05-15), with per-job state, timing and agent. Logs for the 262 failed jobs in the 194 merge-group builds since 2026-09-01 that had a failed job. All builds of any branch from 2026-11-04 to 2026-11-18 (2,965) for comparison.
- **Git objects.** A bare clone in a scratch directory. The 614 merge-group commits named by removal events and the 130 PR heads at conflict ejection were fetched by SHA. All were still retrievable.
- **Engine.** `boss engine ci list --json` (1,196 CI-remediation rows, 304 of them queue ejections for mono) and `boss cost top` (per-run token counts since 2026-10-18).
- **Bazel.** `bazel query 'deps(kind(".*_test rule", //...))'` at revision `68852b4f1af1`: 232 test targets, 151 packages, 3,003 first-party nodes.

**Methods.**

- **Did an ejection need a change?** The `beforeCommit` on a removal event is the queue's squash commit for that PR, not the PR head. For a merged entry it is the commit on `main`. For each failed-check ejection I diffed that squash commit against its parent and compared the added and removed lines, file by file, with the same PR's eventually merged commit. Identical means the PR landed later without any change to its content.
- **Why did the build fail?** Each failed job log was reduced to its error lines and classified by signature: rustc errors, Bazel test failures and timeouts, checkleft findings, checkleft subprocess timeouts, and infrastructure (remote execution failure, unresolvable host, agent loss, missing tool). A build takes its most deterministic class.
- **What kind of conflict?** For each merge-conflict ejection I recovered the PR head from force-push events and ran `git merge-tree` against `main` as it stood at the ejection, then parsed the diff3 hunks. This repeats the method of the [conflict investigation](merge-conflict-sources-and-remedies.md) on the queue's own ejections.
- **Affected tests.** A changed file maps to the test targets whose transitive dependencies include it. A change to a `BUILD.bazel` or `.bzl` file affects every test that depends on the package. A change to `MODULE.bazel`, `MODULE.bazel.lock`, `.bazelrc`, the toolchain files, `third_party/`, `tools/test-sandbox/` or `.buildkite/` affects everything.

**Limits.**

- The cause classification is a signature heuristic. I read about twenty logs by eye. One early rule misread Swift test output as a compile error and was corrected; the tables use the corrected rule.
- "Merged later with an identical patch" proves the PR needed no change. It does not prove a flaky test: `main` could have been fixed in between. The cross-check is the independent flake floor in [Finding 4](#4-why-entries-are-ejected-for-failed-checks).
- "Merged later with a changed patch" is an upper bound on real failures. It includes PRs that were edited for another reason while out of the queue.
- The affected-test estimate applies today's graph to six weeks of history, works at file and package granularity, and ignores implicit and toolchain edges. It is a planning number, not a soundness result.
- Run-cost figures come from a top-5,000 list by token count, so cheap runs are missing and the medians are biased upward.
- Web sources describe vendor products that change monthly. Treat product details as of the retrieval date.

## Baseline

### 1. What merges mono today

[measured] [code]

- **GitHub's native merge queue**, enforced by a ruleset on `main` created on 2026-05-07: squash merge, up to 5 entries built concurrently, groups of 1 to 5 merged at a time, "all entries must pass", 60-minute check timeout, no bypass actors, no required approvals.
- **Three required checks**, all Buildkite commit statuses: `buildkite/mono/bazel-build-test`, `buildkite/mono/mac-app-build` and `buildkite/mono/checks` (`.buildkite/REQUIRED_CHECKS.md`). `bazel-build-test` builds and tests `//...`, less the macOS app and installer packages, with no change scoping (`.buildkite/steps/bazel-build-test.sh`).
- **The engine issues the merge** as `gh pr merge --auto --squash --match-head-commit <sha>` under the operator's own `gh` credentials (`tools/boss/engine/core/src/merge_when_ready.rs`). The product's merge mechanism is `Direct`, which covers GitHub's queue transparently. The only other mechanism is `TrunkQueue` (`tools/boss/engine/storage-values/src/merge_mechanism.rs`).
- **Every entry was enqueued by one account.** Two people authored the 3,571 PRs, one of them 6. There are no third-party mergers today.
- **flunge uses Trunk's queue.** GitHub's queue is only available to organisation-owned repositories, and flunge is user-owned [web] ([GitHub](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)).

### 2. How often entries are ejected

[measured] Queue entries by the month they were enqueued. November covers 18 days.

| Month | Entries | Merged | Ejected: failed checks | Ejected: merge conflict | Share ejected for checks | Share ejected for conflict | Merged PRs ejected at least once |
| ----- | ------: | -----: | ---------------------: | ----------------------: | -----------------------: | -------------------------: | -------------------------------: |
| May   |     712 |    672 |                     29 |                      10 |                     4.1% |                       1.4% |                             4.2% |
| Jun   |     590 |    511 |                     56 |                      17 |                     9.5% |                       2.9% |                            11.9% |
| Jul   |   1,025 |    887 |                     96 |                      34 |                     9.4% |                       3.3% |                            11.3% |
| Aug   |     252 |    212 |                     28 |                      12 |                    11.1% |                       4.8% |                            15.4% |
| Sep   |     219 |    191 |                     18 |                       9 |                     8.2% |                       4.1% |                            11.1% |
| Oct   |     437 |    350 |                     67 |                      18 |                    15.3% |                       4.1% |                            18.3% |
| Nov   |     363 |    274 |                     52 |                      32 |                    14.3% |                       8.8% |                            20.4% |

Over the whole period: 3,598 entries, 3,097 merged, 346 ejected for failed checks, 132 for a merge conflict, 15 removed by hand, 2 timed out, 2 cleared, 4 still queued at export.

The ejection rate has quadrupled since May while throughput fell. July landed 887 PRs with 13% of entries ejected. November is landing about half that rate with 23% ejected. This is not a volume effect.

### 3. Where the time goes

[measured] From the first merge request to the merge, for PRs first enqueued in each window.

| Window          | PRs merged | Median | 90th pct | 95th pct | Never ejected: median, 90th pct | Ejected at least once: median, 90th pct | Share of total wait spent by ejected PRs |
| --------------- | ---------: | -----: | -------: | -------: | ------------------------------: | --------------------------------------: | ---------------------------------------: |
| May 8 to Nov 17 |      3,061 |  3.1 m |   20.5 m |   64.5 m |                    2.8 m, 6.4 m |                            48 m, 23.7 h |                                      98% |
| Oct 1 to Nov 17 |        590 |  5.4 m |   67.4 m |    226 m |                    4.8 m, 8.8 m |                            68 m, 23.8 h |                                      97% |

- 358 ejected PRs spent 7,927 hours between merge request and merge. 2,703 clean PRs spent 191 hours in total.
- The queue's own build is fast because Bazel's caches carry over from the PR build. A passing merge-group build takes a median of 4.3 minutes (90th pct 8.4) in November, up from 1.4 in May. A PR-branch build takes a median of 4.9 minutes (90th pct 7.9).
- The queue is a small part of a PR's life. Since October the median PR waits 4.7 hours between creation and its first merge request (90th pct 52 hours). The [conflict investigation](merge-conflict-sources-and-remedies.md) shows that this lifetime is what drives in-review conflicts.

### 4. Why entries are ejected for failed checks

**Half of them needed no change.** [measured] Of 345 failed-check ejections with a recoverable queue commit:

| Outcome                                      | Ejections | Share | Ejection to merge: median | 90th pct |
| -------------------------------------------- | --------: | ----: | ------------------------: | -------: |
| Merged later with a byte-identical patch     |       161 | 46.7% |                      21 m |    4.7 h |
| Merged later after the patch changed         |       168 | 48.7% |                      58 m |   25.3 h |
| Merged later with only a lockfile difference |         3 |  0.9% |                           |          |
| Not merged yet                               |        13 |  3.8% |                           |          |

The identical share was 38% in May, 46% in July, 67% in October and 43% in November.

**By cause.** [measured] The 135 ejections from 2026-09-01 to 2026-11-18 that match a failed build with logs:

| Cause in the failing build                 | Ejections | Merged later unchanged | Merged after a change | Not merged yet |
| ------------------------------------------ | --------: | ---------------------: | --------------------: | -------------: |
| Test failure                               |        93 |                     58 |                    33 |              2 |
| checkleft finding                          |        12 |                      4 |                     5 |              3 |
| Infrastructure                             |        11 |                      5 |                     1 |              5 |
| Compile error                              |         8 |                      0 |                     8 |              0 |
| Test timeout                               |         7 |                      4 |                     3 |              0 |
| checkleft subprocess timeout (`lint/rust`) |         4 |                      2 |                     0 |              2 |
| **Total**                                  |   **135** |                 **73** |                **50** |         **12** |

- **Compile errors are the queue doing its job.** All 8 needed a change. The recurring shapes are a new required field (`error[E0063]: missing field`) and a changed function arity (`error[E0061]`): one PR changed a signature and another added a caller.
- **checkleft findings that need a change are the same thing in a lint.** In the four `file/size` cases a file passed the limit on the PR and exceeded it once combined with what had landed since.
- **Test failures are mostly not the PR.** 58 of 93 landed unchanged.

**The flake floor, measured independently.** [measured] GitHub fast-forwards `main` to the exact commit the queue tested, and Buildkite then builds that commit again on `main`. From 2026-11-04 to 2026-11-18:

| Build of a commit that already passed the queue | Builds | Failed | Rate |
| ----------------------------------------------- | -----: | -----: | ---: |
| Push-triggered build on `main`                  |    216 |     12 | 5.6% |
| Scheduled build on `main`                       |  1,428 |     96 | 6.7% |

81 of those 108 failures are in `bazel-build-test`. Most test results in these builds come from cache, so the rate for a build that really re-runs tests is higher. This matches the queue itself: with nothing ahead in the queue, 7.8% of entries are still ejected for failed checks, and none for a conflict.

**The flakiness is diffuse.** [measured] The failed builds contain 147 distinct failing test cases; 17 appear in three or more PRs. `//tools/boss/engine/core:engine_lib_test` is in 34 of the 73 spurious ejections. The three worst cases (two `remote_wrapper` tests and one `read_admission` burst test) cover 18 of the 73, and `EngineReconnectIntegrationTests` another 5. One remediation note describes a typical case: a 10-second poll for a file written by a background process, on a host whose "load average is ~23". Fixing the top few tests helps; it will not remove the class.

**Fate sharing does not eject anyone.** [measured] In all 345 ejections the commit under the ejected entry is on `main`: every entry ahead of it had merged. GitHub never ejected a PR because of a failure in front of it. What fate sharing does cost is rebuilds: 59 of the 194 failed merge-group builds since September belong to groups that were rebuilt, not ejected.

### 5. Merge-conflict ejections

[measured] 132 ejections, replayed against `main` at the moment of ejection:

| Conflict consists of                                 | Ejections | Share | Ejection to merge: median | 75th pct |
| ---------------------------------------------------- | --------: | ----: | ------------------------: | -------: |
| Lockfiles only: regenerate                           |        23 |   17% |                      47 m |    2.4 h |
| Same-spot insertions only: keep both                 |        30 |   23% |                     106 m |   20.3 h |
| At least one hunk where both sides edited same lines |        73 |   55% |                      84 m |    6.9 h |
| Did not reproduce                                    |         6 |    5% |                           |          |

- `MODULE.bazel.lock` is in 30 of the 126 reproduced conflicts. The next files are the schema migration chain (`schema_init.rs` 8, `schema_migration_registry.rs` 7). 94 of 126 involve one file. The mix matches the wider corpus in the conflict investigation (45% mechanical).
- **These are conflicts between queue-mates.** No entry that found the queue empty was ejected for a conflict. With one or more entries ahead the rate is 8% to 13%.
- **GitHub finds the conflict late.** 116 of 132 were ejected a minute or more after enqueueing (median 173 seconds), which is when the entry ahead merged. The conflict was knowable at enqueue time: both heads existed.
- November has 32 conflict ejections in 18 days, against 18 in all of October.

### 6. Queue occupancy

[measured]

| Entries already in the queue at enqueue | Share of enqueues | Ejected: failed checks | Ejected: conflict |
| --------------------------------------- | ----------------: | ---------------------: | ----------------: |
| 0                                       |             59.5% |                   7.8% |              0.0% |
| 1                                       |             23.0% |                  12.6% |              8.1% |
| 2                                       |              9.9% |                  12.6% |             10.1% |
| 3                                       |              4.3% |                  11.1% |              8.5% |
| 4 or more                               |              3.3% |                  11.9% |             12.7% |

- The deepest queue seen had nine entries ahead of a new one. The busiest hour had 19 enqueues and the busiest day 117 (2026-07-24). Since October the busiest hour had 15 and the median active hour 2.
- With a 4.4-minute build and 5 concurrent builds, the queue can land about 68 PRs an hour when everything passes [estimate]. Demand peaks at a quarter of that.
- An entry that shares the queue is ejected about three times as often as one that does not (20% to 25% against 7.8%). As volume grows, every entry shares the queue. That is the mechanism behind the owner's concern, and it is already visible.

### 7. How separable is the build graph

[estimate] For the 479 entries merged since 2026-10-18, using the method above:

- The median PR affects 34% of the 232 test targets. 21% of PRs touch a file that affects everything; `MODULE.bazel.lock` alone is in 16% of PRs. 6% affect no test.
- **Of 274 pairs of PRs that were in the queue together, 27 (10%) have disjoint affected tests.** 18 of 156 entries that had something ahead of them (12%) were independent of everything ahead.
- Treating `MODULE.bazel.lock` and Cargo manifests as affecting nothing, which is unsound, leaves the pair figure at 10%. The lockfile is not what couples PRs.
- The coupling is the hub crates. `tools/boss/protocol` is in the dependency closure of 107 of 232 tests, `tools/boss/engine/utils` of 99, `lib/rust/process_runner` of 92.

Uber reported the same effect: its conflict analyzer improved turnaround by up to 60% in the best case but the deep iOS graph produced "a large number of conflicts among changes" [web] ([paper](https://lethain.com/static/blog/2019/eurosys19uber.pdf)).

### 8. What recovery costs today

[measured] [code]

- **A failed-check ejection always starts an agent.** The engine files a `merge_queue_rebounce` remediation and spawns a `ci-fix` revision. Its only pre-triage is "retrigger" when every failure is a startup failure or a cancellation (`classify_pre_triage`, `tools/boss/engine/merge-poller/src/ci_watch.rs`). The worker rebases, verifies, and re-enqueues the PR itself with `enqueuePullRequest`; the engine audits the receipt afterwards (`tools/boss/docs/runbooks/merge-queue-ci-verification.md`). `CiLogReader::retrigger` exists and has no production caller.
- **For an ejection that needed no change, that loop takes a median of 13 minutes to re-enqueue** (75th pct 42 minutes, 90th pct 4 hours) and 21 minutes to land.
- **Each such run is expensive for what it does.** 65 queue-ejection revision runs with cost records since 2026-10-18 wrote a median of 7,400 output tokens and processed a median of 4.0 million tokens, mostly cache reads. 293 conflict-resolution runs: 6,800 and 3.5 million. Each is charged against the per-PR CI attempt budget (default 3), and refunded only when the worker records a rebase-only success.
- **A conflict ejection returned the PR to review** until this week. See [Relationship to merge intent](#relationship-to-merge-intent).

### 9. flunge on Trunk

[measured] 512 `trunk-merge/*` builds for 488 PRs since 2026-07-20, about four a day. 17 failed (3.3%). A passing build takes a median of 1.0 minute. The engine's ledger has 20 Trunk evictions.

At four merges a day the queue is almost always empty. flunge tells us Trunk works and that the engine's Trunk integration works. It tells us nothing about whether Trunk's batching or lanes are better in practice.

### 10. What a smarter queue would remove

[estimate] Applied to the 157 ejections among the 747 entries enqueued between 2026-10-01 and 2026-11-17 (21%):

| Capability                                      | Ejections it addresses | Hours those spent between ejection and merge | Basis                                                                 |
| ----------------------------------------------- | ---------------------: | -------------------------------------------: | --------------------------------------------------------------------- |
| Retry once before ejecting                      | 65 of 110 failed-check |                                          289 | The PR landed later unchanged                                         |
| Deterministic conflict resolution, then requeue |      20 of 47 conflict |                                          112 | 10 lockfile-only, 10 same-spot insertions                             |
| Bounded fix of ten lines or fewer, then requeue |  9 of 110 failed-check |                                           63 | The PR landed after ten changed lines or fewer                        |
| Detecting conflicts at enqueue time             |     Latency, not count |                                              | 88% of conflict ejections were found only when the entry ahead merged |
| Bazel-aware batching or lanes                   |                      0 |                                            0 | No collateral ejections; 10% of co-queued pairs independent           |
| **Total addressed**                             |          **94 of 157** |                             **464 of 1,664** | Upper bound: no tier will be fully effective                          |

That would take the ejection rate from 21% to about 8%.

It addresses 60% of the ejections but only 28% of the hours. The long waits belong to the 31 failed-check ejections that needed a larger change (944 hours) and the 27 conflicts where both sides edited the same lines (256 hours). There the time is an agent's work, a review, or a person's attention, and no queue removes it.

## Testing the thesis

**Claim 1: standard queues batch badly, GitHub's especially.** Not supported for mono. GitHub's queue does not batch CI at all: each entry gets its own build on top of the entries ahead, up to the concurrency limit [web] ([GitHub](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue)). That design is wasteful when an entry ahead fails, because everything behind is rebuilt, but it has not cost us a single ejection, and the rebuilds are 4-minute builds. Whether Trunk's batching is better in practice cannot be answered from flunge.

**Claim 2: queues cannot resolve conflicts, and many of ours are trivial.** Supported. 40% of queue conflict ejections are mechanical, close to the 45% measured for in-review conflicts, and each costs most of an hour or more. No hosted queue resolves them for a Bazel repo (see [Prior art](#prior-art)).

**What the thesis leaves out.**

- **Flakes are the biggest cause, and a queue amplifies them.** The queue adds a second full build to every PR, and a build of known-good code fails 6% to 8% of the time. Over six months that produced about one spurious ejection for every real catch (161 against at most 168). Every system that runs a gate at scale has an explicit answer to this (Chromium's retry and "without patch" run, Shopify's consecutive-failure threshold, Trunk's pending-failure depth). Ours has none.
- **The cost is in the recovery loop, which is ours.** GitHub ejects in seconds. The 13 to 106 minutes that follow are the engine starting an agent, or waiting for a person.
- **The graph is too connected for lanes to help yet.** This is the part of the "holy grail" with the weakest support.
- **The ejection probability, not the scheduler, limits throughput at scale.** A chain of speculative builds lands entries until the first failure. With a per-entry ejection probability `p` the expected run of successes is `(1 - p) / p`: four entries at today's 21%, eleven at 8%. Reducing `p` is what makes speculation, batching and lanes worth having later.
- **Is the queue doing more harm than good?** [estimate] No. It stopped up to 168 changes that would have broken `main` in six months, about 1 in 20 merges. With more than 100 PRs open and agents that rebase onto `main` and try to fix whatever is red, a broken `main` is much more expensive than a flaky ejection. The right response is to remove the false ejections, not the gate.

## Prior art

### Hosted queues

[web] Retrieved 2026-11-18. "Lanes" means testing independent PRs in parallel without stacking them. The last column is this document's assessment.

| Product            | Speculation and batching                                                         | Failed batch                         | Lanes from affected targets                                                               | Conflicts                                                                                        | Flakes                                                                                                                          | Where it falls short for us                                                                                              |
| ------------------ | -------------------------------------------------------------------------------- | ------------------------------------ | ----------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| GitHub merge queue | One build per entry stacked on those ahead, 1 to 100 concurrent. No CI batching. | Entry removed, groups behind rebuilt | None. One queue per branch.                                                               | Removed                                                                                          | None, unless "only merge non-failing pull requests" is turned off, which lets a later green group cover an earlier red one      | No retry, no resolution, organisation repos only, late conflict detection                                                |
| Trunk              | Predictive testing, optional batching (suggested size 4), optimistic merging     | Halving bisection                    | Yes. Targets uploaded per head SHA; `"ALL"` serialises. Queue waits until targets arrive. | Not admitted until mergeable. Any push ejects. No resolution.                                    | Pending-failure depth: a failed entry waits to see whether entries behind it pass                                               | Flake protection needs entries behind you, which we rarely have. No resolution. Upload is a silent gate.                 |
| Mergify            | Parallel checks (default 5), batches of 1 to 128, scopes as lanes                | Recursive split, configurable        | Yes. Scopes from file patterns or uploaded; a Bazel guide and a Buildkite plugin exist.   | Deterministic `conflict_resolvers` in preview: lockfile, language-aware, path union. Not an LLM. | `max_checks_retries` re-runs queue CI. Automatic job retry is GitHub Actions only.                                              | Resolvers are preview-only, cannot run `bazel mod deps`, and never push into a queued PR. Lanes batch only equal scopes. |
| Aviator            | Parallel mode with optimistic validation, batching                               | Halving bisection                    | Yes, by API. Computing targets is left to the user.                                       | Blocked; the author must fix                                                                     | Beta: a deterministic ladder, then a model asked one question, then one rerun of the failed job. Never marks a check as passed. | Cannot retry checks reported as commit statuses, which ours are. Conflicts need a person.                                |
| Graphite           | Stack-aware, parallel CI. Batching in private beta.                              | Parallel isolation or bisection      | None                                                                                      | Removed. Their guide says the queue "is not AI-powered".                                         | None                                                                                                                            | No lanes, no resolution. Now owned by Cursor.                                                                            |

Sources: GitHub ([queue](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/managing-a-merge-queue), [rules API](https://docs.github.com/en/rest/repos/rules)); Trunk ([parallel queues](https://docs.trunk.io/merge-queue/optimizations/parallel-queues), [impacted targets API](https://docs.trunk.io/merge-queue/optimizations/parallel-queues/api.md), [batching](https://docs.trunk.io/merge-queue/optimizations/batching.md), [anti-flake](https://docs.trunk.io/merge-queue/optimizations/anti-flake-protection.md)); Mergify ([conflict resolution](https://docs.mergify.com/merge-queue/conflict-resolution.md), [scopes](https://docs.mergify.com/configuration/scopes.md), [Bazel](https://docs.mergify.com/configuration/scopes/bazel.md), [batch failures](https://docs.mergify.com/merge-queue/batches/failures.md), [auto-retry](https://docs.mergify.com/ci-insights/auto-retry.md)); Aviator ([flaky test management](https://docs.aviator.co/mergequeue/concepts/flaky-test-management.md), [affected targets](https://docs.aviator.co/mergequeue/concepts/affected-targets.md), [lifecycle](https://docs.aviator.co/mergequeue/concepts/pull-request-lifecycle)); Graphite ([optimisations](https://graphite.com/docs/merge-queue-optimizations), [setup](https://graphite.com/docs/set-up-merge-queue), [acquisition](https://cursor.com/blog/graphite)).

Observations:

- **All three lane-capable queues get Bazel targets the same way:** [bazel-diff](https://github.com/Tinder/bazel-diff) hashes at two revisions, uploaded per head SHA. That is the cumbersome part of Trunk's setup, and it is the same work in Mergify and Aviator.
- **Nobody puts an LLM in the queue to resolve conflicts.** The nearest features are Mergify's deterministic resolvers, Aviator's model-gated rerun, and GitHub's Copilot "Agent Merge", which fixes failing Actions workflows and conflicts on a PR before it is queued ([changelog](https://github.blog/changelog/2026-10-28-agent-merge-watches-and-fixes-pull-requests-in-the-cloud)).
- **Vendors are converging on exactly our two gaps**, retry and resolution, and both are in preview or beta. This is a reason to keep the queue core replaceable.
- **Published scale thresholds are above us.** Mergify says GitHub's queue is "fine at 10 PRs a day" and strains at 20 to 30 a day with 30-minute CI ([blog](https://mergify.com/blog/when-to-outgrow-github-merge-queue)). Trunk suggests batching at 50 or more PRs a day and advises against optimistic merging at one or two PRs an hour ([docs](https://docs.trunk.io/merge-queue/optimizations/optimistic-merging.md)). Mergify's 2026 survey of about 160 organisations found 94% of private merges still go through one PR at a time ([report](https://mergify.com/reports/state-of-merge-queues-2026)).
- **GitHub's queue has had correctness incidents.** On 2026-04-23 squash merge groups with more than one PR produced merge commits that reverted earlier changes, affecting 2,092 PRs ([status](https://www.githubstatus.com/incidents/zsg1lk7w13cf)). mono's own queue stalled on 2026-10-14.

### Systems built in-house and in the open

[web]

| System           | Speculation                                                    | Batching and bisection                                   | Independence                                                        | Textual conflicts        | Flakes                                                                         | Safety net after merge                        |
| ---------------- | -------------------------------------------------------------- | -------------------------------------------------------- | ------------------------------------------------------------------- | ------------------------ | ------------------------------------------------------------------------------ | --------------------------------------------- |
| Uber SubmitQueue | Model-ranked subset of the tree of possible builds             | None in the paper                                        | Target-hash conflict graph; independent changes land in parallel    | Rejected                 | Assumes hermetic, deterministic tests                                          | None by design                                |
| Zuul             | Optimistic chain inside a window that grows and halves         | None                                                     | Statically configured shared queues                                 | Reported and dequeued    | Manual `recheck`; a flake resets everything behind it                          | None built in                                 |
| rust-lang bors   | None. Strictly serial, about 2 to 3 hours a run.               | Hand-made rollups of 10 to 20 PRs; culprit found by hand | None                                                                | Author rebases           | A person types `@bors retry`                                                   | Performance triage of rollups                 |
| bors-ng          | None                                                           | Batch everything waiting; halve on failure               | None                                                                | Author rebases           | None                                                                           | None. Archived in 2024.                       |
| Kubernetes Tide  | One batch and one serial PR at a time                          | Greedy batch of cleanly merging PRs; no bisection        | Per repo and branch                                                 | Left out of the batch    | Not addressed                                                                  | Outside Tide                                  |
| Chromium CQ      | None. Test against tip, then land; results reusable for a day. | Only declared dependent changes                          | Path filters and test selection                                     | Rebased for the try jobs | Retry failed shards, then re-run without the patch and ignore what still fails | Tree closure, gardeners, automatic revert     |
| Google and Meta  | None before merge                                              | Batched post-submit runs with automated culprit finding  | Affected tests from the build graph; learned test selection at Meta | Land rejected            | Flaky tests kept out of presubmit; fail only on repeated failure               | The main mechanism: detect, bisect, roll back |
| Shopify (2019)   | Optimistic "predictive branch"                                 | Batches of 8, three at a time                            | None                                                                | Not described            | Eject only on the fourth consecutive failure                                   | Queue locks on incidents                      |

Sources: Uber ([paper](https://lethain.com/static/blog/2019/eurosys19uber.pdf), [bypassing large diffs](https://www.uber.com/en-US/blog/bypassing-large-diffs-in-submitqueue/), [CI at scale](https://arxiv.org/abs/2501.03440), [open-source rewrite](https://github.com/uber/submitqueue)); Zuul ([gating](https://zuul-ci.org/docs/zuul/latest/gating.html), [pipeline](https://zuul-ci.org/docs/zuul/latest/config/pipeline.html)); rust-lang ([CI](https://rustc-dev-guide.rust-lang.org/tests/ci.html), [rollups](https://forge.rust-lang.org/release/rollups.html)); [bors-ng](https://github.com/bors-ng/bors-ng); Tide ([docs](https://docs.prow.k8s.io/docs/components/core/tide/)); Chromium ([CQ](https://chromium.googlesource.com/chromium/src/+/main/docs/infra/cq.md)); Google ([Software Engineering at Google, ch. 23](https://abseil.io/resources/swe-book/html/ch23.html)); Meta ([predictive test selection](https://arxiv.org/abs/1810.05286)); Shopify ([2019](https://shopify.engineering/successfully-merging-work-1000-developers)).

What transfers:

- **Uber's numbers describe a different regime.** Their single queue fails at 1,000 changes a day with 30-minute builds. Their learned predictor pays off at 100 to 500 changes an hour. At 100 an hour their system matches an oracle that knows every outcome in advance; so does any simple chain. We are at 15 an hour at the peak with 4-minute builds.
- **Uber's independence rule is stricter than "disjoint targets".** Two changes can conflict without sharing a target when one of them changes the build graph. The full test is that the targets affected by both changes together equal the union of those affected by each alone. 7.9% of their iOS changes altered the graph. None of the hosted queues documents this check.
- **Uber later found that speculation wastes most builds.** 40% to 65% of builds were being abandoned; adding build-time prediction and a speculation threshold cut CI resource use by 53% ([paper](https://arxiv.org/abs/2501.03440)).
- **Every in-house gate has a flake answer; every one bounces textual conflicts back to the author.** Automatic resolution in the queue would be new. That is an opportunity, and a reason for care: there is no operating experience to borrow.
- **Detect-and-revert is the alternative to gating**, and it needs fast culprit finding, automated rollback and people on watch. Google reports hours of broken-trunk exposure per breakage and that naive per-change culprit accuracy of 77% would mean 20 to 130 wrong reverts a day ([SafeRevert](https://hackthology.com/saferevert-when-can-breaking-changes-be-automatically-reverted.html)). With 4-minute CI a gate is the cheaper guarantee for us.
- **Batching has a simple optimum.** With per-change failure probability `p`, testing batches of `b` and falling back to individual tests costs about `1/b + 1 - (1 - p)^b` builds per change, minimised near `b = 1/sqrt(p)` [estimate]. At `p` of 20% that is a batch of 2 and a saving of about 15%. At 5% it is a batch of 4 or 5 and 2.3 times the capacity. An empirical study found that a fixed batch of 4 captures most of the benefit when fewer than 40% of builds fail ([Beheshtian et al.](https://conf.researchr.org/details/icse-2022/icse-2022-journal-first-papers/61/Software-Batch-Testing-to-Save-Build-Test-Resources-and-to-Reduce-Feedback-Time)).

### Automatic resolution

[web]

- **Lockfiles.** Bazel documents a `jq` merge driver for `MODULE.bazel.lock` and otherwise says to fix `MODULE.bazel` and re-run `bazel mod deps` ([docs](https://bazel.build/external/lockfile)). The rules_rust behaviour that embeds Cargo manifest hashes in the lockfile is a known source of "widespread and unnecessary merge conflicts" ([BazelCon 2025](https://hofbi.github.io/events/bazelcon_2025_cargo_splicing/)). Renovate's rule is the right one for any generated file: regenerate from the base, never merge ([docs](https://docs.renovatebot.com/updating-rebasing/)).
- **Structured merge.** [Mergiraf](https://mergiraf.org/languages.html) supports Rust, Starlark, TOML, JSON and YAML but not Swift. Replaying Linux kernel history it fully resolved 428 of 7,415 conflicting merges ([LWN](https://lwn.net/Articles/1042355/)). More structure also means more wrongly accepted merges ([study](https://arxiv.org/abs/2608.11345)).
- **Models.** On Merge-Bench (7,938 real hunks, mid-2025 models) the best model matched the human resolution 55% of the time and produced different code 34% of the time; the authors note that a wrong resolution is worse than leaving the conflict ([paper](https://arxiv.org/html/2605.25890v1)).
- **The characteristic failure is silent deletion.** Public reports describe resolvers that dropped a PR's core fix or deleted the base side's tests while CI stayed green ([example](https://github.com/stSoftwareAU/VibeCoder/issues/3757)). This is [incident 002](../../tools/boss/docs/postmortems/incident-002-merge-conflict-deletion-blessed-by-review.md) in this repo, and the engine's both-parents deletion tripwire is the matching guard.
- **Flake triage.** Chromium's rule is the cleanest: retry, then run the failing tests without the patch, and ignore what fails either way ([CQ](https://chromium.googlesource.com/chromium/src/+/main/docs/infra/cq.md)). Bazel's `--flaky_test_attempts` hides the retry inside the build and reports the target as passing, which loses the signal.

## Design space

This section describes a queue core we might build. The recommendation is to build it later, if at all; the analysis is here so the earlier phases do not paint us into a corner.

### Speculation, batching and bisection

| Option                                                          | Throughput when all pass | Cost of a failure              | Complexity | Fit                                                               |
| --------------------------------------------------------------- | ------------------------ | ------------------------------ | ---------- | ----------------------------------------------------------------- |
| Serial: one candidate at a time                                 | `1/T`: 13 an hour        | One build                      | Lowest     | Carries today's median hour, not the peak                         |
| Optimistic chain of depth `k`, restart behind the first failure | `k/T`: 68 an hour at 5   | About `k/2` builds             | Low        | What GitHub and Zuul do. Enough for several times today's volume. |
| Chain plus batches of `b` with bisection                        | `k*b/T`                  | About `2*log2(b)` extra builds | Medium     | Pays once `p` is under about 10% and the chain is regularly full  |
| Ranked speculation over many possible futures                   | Near optimal             | Many abandoned builds          | High       | Uber scale only                                                   |

`T` is the queue build time, 4.4 minutes at the median.

[opinion] An optimistic chain with a Zuul-style window is the right core if we build one: grow the window by one per landed entry, halve it on an ejection, floor of 2. It is a small state machine, its behaviour is well understood, and it degrades to serial on a bad day. Batching is a later, additive option because a batch is just a chain link holding several PRs.

Two things would be worth doing differently from GitHub from the start:

- **Check for conflicts at admission.** When a PR is submitted, merge it against `main` and against each entry ahead. A conflict is known within a second, not three minutes later when the entry ahead lands.
- **Resolve speculatively.** If the conflict is in a deterministic class, resolve it against the entry ahead while that entry is still testing, and queue the resolved candidate behind it. If the entry ahead is then ejected, the resolution is discarded and the original PR is tested as it was.

### Partitioning by affected targets

**Computing the sets.**

- Use bazel-diff, pinned to one version, hashing `main`'s tip and the candidate (the PR applied to that tip, not the PR head). It handles bzlmod and by default treats a module extension as standing in for every repository it generates, so a lockfile change touches most of the workspace [web] ([README](https://github.com/Tinder/bazel-diff)). [target-determinator](https://github.com/bazel-contrib/target-determinator) is configuration-aware but slower (minutes on a mid-sized repo) and coarser on bzlmod. Plain `rdeps(//..., set(files))` is unsound: a change to a `BUILD` or `.bzl` file has no changed target to start from.
- Compute in CI as a step of the candidate's build and publish the result as an artifact. The hashes for `main`'s tip are cacheable per commit.
- **Global barrier.** Any change to `MODULE.bazel`, `MODULE.bazel.lock`, `.bazelrc`, `.bazelversion`, `rust-toolchain.toml`, the root `BUILD.bazel`, `third_party/`, `platforms/`, `tools/test-sandbox/`, `.buildkite/` or `CHECKS.yaml` affects everything. In our data that is 21% of PRs.
- **Fail closed.** A query error, a timeout, a missing result or an oversized set means "everything".

**Soundness.**

- Disjoint sets are not enough when either change alters the build graph. Either apply Uber's union-graph test, or treat any `BUILD` or `.bzl` change as global. The second is simpler but expensive here: about a quarter of PRs touch a build file, on top of the 21% that touch a global file.
- No hash sees undeclared inputs, non-hermetic tests or registries discovered at run time. The backstop is the build of `main` that already runs after every merge, plus a rule that a deterministic failure there freezes the lanes and reverts the most recent lane merges through the queue.
- The checks step is a second partition. checkleft scopes itself to changed lines, and a finding can depend on two PRs together, as with `file/size`.

**What our graph says.** With 10% of co-queued pairs independent, lanes would let about one entry in eight skip ahead. The benefit grows only if the graph gets flatter, which the ongoing crate extraction is doing, or if PRs get smaller. The measurement in [Finding 7](#7-how-separable-is-the-build-graph) is cheap to repeat and should gate this work.

### Parallel lanes and the trunk tip

Independent entries can be tested against `main`'s tip in separate lanes. The question is what happens when lane A lands while lane B is still testing against the old tip.

| Policy                                                   | Guarantee                                                     | Cost                                      |
| -------------------------------------------------------- | ------------------------------------------------------------- | ----------------------------------------- |
| Retest B on the new tip                                  | Every landed tree was tested exactly                          | Lanes save nothing                        |
| Land B without retesting, trusting independence          | Each change tested; the combination is not                    | Soundness rests on the independence test  |
| Land B, and build the combined tip afterwards as a check | Same, plus detection within one build and an automatic revert | One build per merge, which we already run |

[opinion] The third policy is the only one that makes lanes worth having, and it changes the guarantee from "`main` is always green" to "`main` is green or is being reverted". That is a product decision for the owner.

One more consequence: with lanes, the commit that lands is not the commit that was tested, so the queue can no longer prove tree equality. Under the single-chain design it can.

### Flakes and real failures

[opinion] Order of preference:

1. **Retry once.** With GitHub's queue, which ejects on the first red status, that means re-enqueueing the same head. With a core of our own it means retrying the failed job on the same candidate; Buildkite can retry a single job, and whether the step's commit status is re-posted on a retry needs confirming on a test branch. Either way a deterministic failure fails again about five minutes later and a flake passes. Our data says this alone removes about half of failed-check ejections.
2. **Exonerate when the build graph allows.** If the failing test target is not in the candidate's affected set, the PR cannot have caused the failure on its own. Retry without counting it against the entry.
3. **Classify infrastructure by signature and never count it.** Remote execution failure, unresolvable host, agent loss, a missing tool on the agent, checkleft's subprocess wall-clock limit.
4. **Keep a flake ledger.** Every pass-on-retry is recorded against the test. The scheduled builds on `main` are a free second source: any failure there is a flake or infrastructure by construction. A test that turns flaky in the PR that touches it is flagged, not forgiven.
5. **Do not hide retries in Bazel.** `--flaky_test_attempts` would make the numbers above disappear without fixing anything.

The risk in any retry policy is a PR that introduces a real intermittent failure and gets through on the second try. The ledger is the mitigation: such a test shows up within days.

### Priorities and fairness

- **Classes.** Urgent (reverts and fixes for a red `main`; jumps the queue and may interrupt running builds), normal, and bulk (dependency bumps, mechanical sweeps).
- **Within a class, first come first served by the time of the merge request**, with one exception: an entry that was ejected and recovered automatically keeps its original time.
- **Barrier entries** serialise everything behind them. Schedule them when the queue is quiet, or land several together.
- **Sibling trains.** Several PRs from one project that touch the same files should land as an ordered chain, each resolved once against its predecessor. The conflict investigation observed three siblings conflicting four to seven times each as they landed in arbitrary order.
- **Risk-aware ordering** (small, leaf changes first) is cheap and was worth about 7% throughput in one Zuul deployment ([BMW](https://arxiv.org/html/2508.08342v1)). It needs an ageing rule so that large changes are not starved.

## Automatic resolution

### Tiers

| Tier              | What it handles                                                                                                                           | Bound                                                                                                                                                                                       | Verified by                                                                                     | Share of today's ejections                  |
| ----------------- | ----------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- | ------------------------------------------- |
| 0. Retry          | Infrastructure signatures; test failures and timeouts, once                                                                               | No change to the PR. One retry per entry.                                                                                                                                                   | The rerun                                                                                       | About half of failed-check ejections        |
| 1. Deterministic  | Lockfile regeneration; keep-both for same-spot insertions of whole items and imports; declared per-path recipes; formatter-only findings  | Only the conflicted files. Every file must resolve or nothing is pushed.                                                                                                                    | Both-parents deletion tripwire, checkleft push gate, the candidate's CI                         | About 40% of conflict ejections             |
| 2. Bounded model  | A conflict left in one file after tier 1; a compile break caused by a base change (new required field, changed arity, renamed item)       | One attempt. Edits only inside conflict hunks, or only on lines the compiler names. A small line budget. No test or assertion removed or weakened, no new dependency, no build-file change. | Tier 1's checks, plus a check that every line each side added survives, plus the candidate's CI | A quarter to a third of real check failures |
| 3. Agent revision | Everything else that looks fixable                                                                                                        | The existing `ci-fix` and conflict revisions                                                                                                                                                | Review                                                                                          | The remainder                               |
| 4. Person         | Both sides changed behaviour; `MODULE.bazel` or toolchain content; migrations; a deletion the tripwire flags; anything that failed tier 2 | None                                                                                                                                                                                        | The person                                                                                      |                                             |

Tier 1 exists today as the conflict ladder's mechanical rungs: `CargoLockResolver`, `BazelModuleLockResolver`, the generated-lock resolvers and `KeepBothUnionResolver` behind the `DeterministicResolver` trait (`tools/boss/engine/deterministic-resolvers/`). The queue must call it, not reimplement it. It also has to work first: since the mechanical tiers became default-on, 6 of 27 resolved lockfile-only conflicts avoided an agent. The conflict investigation's first two recommendations (diagnose against current `main`; record why the ladder fell through) are prerequisites for everything in this section.

### Bounding, verifying and auditing a model's change

- **The model proposes; code decides.** The fixer returns a patch. Deterministic validators check the line budget, the path list, the survival of both sides' additions, and the absence of deleted tests. Only a patch that passes is pushed, and only the queue's own code can merge.
- **The patch is small enough to check mechanically.** "Trivial" is enforced as a budget of about ten changed lines in files the compiler or the conflict already names. In our data 55 of 168 real recoveries fit that budget, and 24 fit in two lines.
- **Every automatic change is its own commit** on the PR, authored by the bot, with trailers naming the tier, the resolver or model, and the ejection it answers. A ledger row links the ejection, the patch and the outcome.
- **Shadow first.** Before the tier may push, run it on every eligible ejection and compare its patch with what the agent revision actually shipped.
- **Budgets.** One attempt per ejection, one automatic recovery of any tier per entry beyond a retry, a wall-clock limit of about one CI cycle, and a token cap two orders of magnitude below today's 4 million per run. The engine's small-model seam (`tools/boss/engine/utility-model/`) is the place for the single-shot calls: flake classification and compile-error repair.
- **Untrusted input.** PR text, logs and diffs are data. The fixer gets no credentials and no network, and its output is constrained by the validators. Automatic tiers apply only to PRs from trusted authors, never to forks.

### What happens when an automatic resolution is wrong

- **It fails to compile or a test fails:** the candidate's CI catches it, the entry is ejected to tier 3, and the bot's commit is dropped.
- **It compiles and passes but is wrong:** this is the residual risk, and CI cannot see it. The mitigations are narrow classes, the deletion tripwire, the survival check, a visible labelled commit, and sampling automatic resolutions for review after they merge. The revert goes through the queue as urgent.
- **It keeps happening in one place:** a per-path circuit breaker turns the tier off for that file and files an attention item. The ladder's churn guard (three attempts an hour) is the model.

### Relationship to merge intent

[code] The engine already records a standing merge request and already resumes a merge after a trivial fix. The queue should consume that, not duplicate it.

| Piece                                                                                         | State                                                                                     | What it does                                                                                                                                                                                                                                                                  |
| --------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `github_merge_intents` ([mono#2885](https://github.com/spinyfin/mono/pull/2885))              | Landed                                                                                    | One row per accepted merge request at an exact head. Retired when the PR merges, closes, is dequeued, or its head advances.                                                                                                                                                   |
| `trunk_merge_intents`                                                                         | Landed                                                                                    | The same for Trunk, and explicitly the authorisation for automatic resubmission after an eviction or a conflict fix, including a fix made by an agent.                                                                                                                        |
| Just-in-time conflict resolution ([mono#3574](https://github.com/spinyfin/mono/pull/3574))    | Landed behind a default-off flag                                                          | Conflicts on PRs nobody has asked to merge are deferred until a merge is requested.                                                                                                                                                                                           |
| Merge-triggered conflict resolution ([mono#3712](https://github.com/spinyfin/mono/pull/3712)) | Landed 2026-11-18; follow-up [mono#3737](https://github.com/spinyfin/mono/pull/3737) open | When a requested merge meets a conflict, the mechanical rungs run, and on a clean result the engine re-issues the merge and records a new intent at the new head. An agent rung, a tripwire halt or a failed regeneration stops the request and returns the card to a person. |
| CI failure on the queue                                                                       | Unchanged                                                                                 | Always an agent revision. The worker re-enqueues.                                                                                                                                                                                                                             |

Three gaps follow from the table:

1. **Intent does not survive a queue ejection for failed checks without an agent.** Tier 0 closes that: on a spurious ejection the engine re-enqueues the same head itself.
2. **The two intent tables have different rules.** On Trunk an agent's fix resubmits automatically; on GitHub only a mechanical fix does. A single rule should say which classes of engine-authored change keep the intent alive.
3. **Intent is checked after the fact.** A queue we own would ask before each landing: is there a live intent for this PR at this head, or at a head derived from the approved one only by recorded changes of an allowed class?

[opinion] The queue should treat the intent store as the authority on "may this land" and keep no approval state of its own. What "an allowed class" includes, in particular whether a tier 2 patch keeps the intent alive, is the main policy question for the owner.

## Correctness and safety

**What keeps `main` green.**

- **Invariant for the single-chain design:** `main` only ever moves to a tree that passed every required check as that exact tree. GitHub's queue gets this by fast-forwarding to the tested commit. A queue that merges through the pull-request API must check after each merge that the new `main` tree equals the tested candidate's tree, and stop the queue if it does not.
- **Nothing else may move `main`.** A ruleset restricts updates to the queue's identity. A merge from outside invalidates every candidate.
- **Retries weaken the invariant slightly** and lanes weaken it deliberately. Both rely on the post-merge build of `main` and an automatic revert.
- **Crash safety.** GitHub is the source of truth for PR state. The queue's own state must be rebuildable from GitHub plus its ledger, and every step must be safe to repeat. The conflict ladder's postmortem lists the same lessons: an attempt needs a lifecycle that survives an engine restart, and "could not run here" must be distinct from "declined".

**Branch protection and required checks.**

- Keep the three required statuses on the PR head, not strict. They remain the admission gate.
- To use our own core on a GitHub repository, remove the `merge_queue` rule and add an update restriction with the queue's identity as the only bypass actor. The engine already recognises a merge rejected by such a restriction and files a `direct_merge_push_restriction` attention item.
- The queue should report its state on the PR as a check of its own, as Trunk does. That check must stay out of the required-failure rollup, which is how the engine already treats Trunk's (`tools/boss/engine/merge-poller/src/classify.rs`).
- A resolution has to be pushed to the PR branch, so that the PR shows what landed. That push re-runs the PR's own CI. With equal trees most of it is a cache hit, but it is a second build and a second chance to flake. On a repository that requires approvals, a bot push also dismisses stale approvals or trips "require approval of the most recent push" [web] ([GitHub](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets)). mono requires none.

**The bot.**

- **A GitHub App, not a personal token.** Today every merge runs under the operator's own `gh` credentials, which also carry the engine's polling. An App gives short-lived tokens, its own rate limit, an identity that rulesets can name, and commits that verify as the App's ([GitHub](https://docs.github.com/en/authentication/managing-commit-signature-verification/about-commit-signature-verification)).
- **Least privilege:** contents and pull requests read and write, statuses and checks read. The bypass covers the update restriction only, never the required checks.
- **Separate the hands.** The process that can merge runs no model. The process that runs a model can only return a patch.
- **Published guidance says the same:** no static tokens near an agent, no secrets in its sandbox, writes staged through checked outputs ([GitHub agentic workflows](https://github.blog/ai-and-ml/generative-ai/under-the-hood-security-architecture-of-github-agentic-workflows/), [OWASP LLM01 and LLM06](https://genai.owasp.org/llm-top-10/)).

## Architecture options

### Engine feature or standalone service

| Question            | (a) Engine feature first                                                                   | (b) Standalone service from day one                                                   |
| ------------------- | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- |
| Who it serves       | PRs the engine manages. That is every PR in mono today.                                    | Any PR, merged by anyone                                                              |
| GitHub interface    | The `gh` subprocess and polling the engine already has; an App identity can be added later | A GitHub App with webhooks. The engine has no webhook receiver and no public ingress. |
| CI interface        | `bk` for results, logs and job retry, as `ci-log-reader` does today                        | Buildkite webhooks or polling; its own credentials                                    |
| State               | Engine database, next to the intent and remediation ledgers                                | Its own store; a second source of truth to reconcile with the engine                  |
| Merge authority     | The engine's intent store                                                                  | Its own approval model: labels, comments or an API                                    |
| Resolution          | The ladder, the resolvers, the tripwire, leased workspaces: all in process                 | Has to call back into the engine, or reimplement them                                 |
| Availability        | Stops when the operator's machine sleeps. So does merging today.                           | Needs an always-on host, deploys, monitoring and an owner                             |
| Multi-repo          | One queue per product; mono and flunge                                                     | Natural                                                                               |
| Cost to first value | Weeks, in small PRs                                                                        | Months before it does anything the current queue does not                             |

[opinion] Choose (a). Everything that produces value in this investigation's data lives in the engine already or needs the engine's state, and there are no third-party mergers to serve. The risk in (a) is that the queue grows into the engine and cannot be lifted out. The seam below is the answer.

### The seam

[code] Today the merge path is a hard-coded `match` on the mechanism in `tools/boss/engine/app/src/app/review.rs`. Observation is split between the GitHub sweep and the Trunk poller in `tools/boss/engine/merge-poller/`. There is no abstraction for "the thing that lands a PR"; the nearest are `DirectMergeExecutor` and `TrunkQueueApi`.

Proposed shape, in two layers:

- **A landing backend** per product: submit an intent, cancel it, and observe entries in normalised states (pending, testing, landed, ejected with a cause, cancelled). GitHub's queue and Trunk become two implementations. Ejection causes are normalised too: GitHub's removal reasons and Trunk's eviction causes map to test failure, conflict, timeout, manual and base mismatch. Each backend declares what it can do: retry in place, check conflicts at admission, lanes.
- **The landing loop** above it, backend-agnostic: on an ejection, pick the tier, run it, and resubmit under the intent. This is where retry policy, the resolver call, the bounded fixer and the ledger live.

A queue core of our own is then a third backend, in its own crate, with ports for everything outside it:

| Port                | Engine adapter                                             | Standalone adapter                       |
| ------------------- | ---------------------------------------------------------- | ---------------------------------------- |
| Code host           | The `tools/boss/github` crate over `gh`                    | GitHub App REST and GraphQL, webhooks    |
| CI provider         | `ci-log-reader` over `bk`                                  | Buildkite API and webhooks               |
| Intent              | `github_merge_intents` and `trunk_merge_intents`           | Labels, comments, or an API call         |
| Resolver            | The deterministic resolvers and the conflict ladder        | The same crates, with its own workspaces |
| Independence oracle | None at first; then bazel-diff results from CI             | The same                                 |
| Store and clock     | The engine database through a narrow DAO; the engine clock | SQLite or Postgres                       |

Per the repo's crate rule, the core crate must not import any engine crate; the engine depends on it, one way. Graduating to (b) is then a new binary around the same crate with the right-hand adapters, plus hosting.

### How a core of our own would land a PR on GitHub

- Build a candidate: the PR squashed onto the predicted base, pushed to a `boss-queue/<base>/pr-<n>-<base-sha>` branch. mono's pipeline builds every branch and posts the three statuses on the commit.
- On green, merge through the pull-request API with squash and an expected-head guard, the same call the engine makes now, so the PR is recorded as merged with its author. Then verify tree equality.
- checkleft needs to recognise the branch shape to recover the PR number, exactly as the Trunk migration investigation found for `trunk-merge/*`.
- The alternative, fast-forwarding `main` to the candidate, makes equality hold by construction but leaves the PR closed and not merged unless its branch is first rewritten to the candidate commit. It is not worth the loss of history on the PR.

## Build, adopt or extend

| Option                                                                | Gets                                                                                                                          | Does not get                                                                                      | Cost                                                                                                                                                                                                                                                                                                            |
| --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Extend:** keep GitHub's queue, build the landing loop in the engine | Retry by re-enqueueing, deterministic resolution with automatic re-merge, the bounded tier, conflict checks before enqueueing | Retry in place, speculative resolution, lanes                                                     | Small engine changes, most of them extensions of code that exists                                                                                                                                                                                                                                               |
| **Adopt Trunk for mono**, plus the loop                               | The above, plus batching, priorities, lanes if we upload targets, independence from GitHub's queue                            | Resolution. Flake protection at our queue depth. A fix for the fact that any push ejects.         | The cutover in the [Trunk investigation](../../tools/boss/docs/investigations/mono-merge-queue-github-to-trunk-2026-10-14.md): one checkleft PR and operator steps. Free for up to five committers [web] ([billing](https://docs.trunk.io/setup-and-administration/billing.md)). The engine integration exists. |
| **Adopt Mergify**, plus the loop                                      | Queue-level CI retries, deterministic conflict resolvers, scopes with a Bazel guide and a Buildkite plugin                    | `bazel mod deps` regeneration. Resolution inside a queued PR. Anything using the engine's intent. | A third queue integration in the engine. The resolvers are in preview, on request.                                                                                                                                                                                                                              |
| **Build the queue core now**                                          | Everything, eventually                                                                                                        | Anything soon                                                                                     | A scheduler, a state machine, a bot identity and ruleset changes, before the first ejection is avoided                                                                                                                                                                                                          |

[opinion] **Extend.** The honest test of "adopt plus a small bot" is whether the small bot is small. It is not: the bot is the retry policy, the resolver ladder, the tripwire, the bounded fixer and the intent rules, which is everything in this document except the scheduler. The scheduler is the commodity part, GitHub's is adequate at our depth, and the engine can already drive Trunk if GitHub's becomes a problem.

A queue core of our own becomes worth building when one of these is true:

- **Depth.** More than 10% of enqueues find five or more entries ahead (1.6% today), or the 90th percentile time in queue for clean entries exceeds three build times (it is about two).
- **Retry cost.** Re-enqueueing after a flake regularly costs more than two build cycles because the entry goes to the back of a deep queue.
- **Reach.** A repository that matters cannot use GitHub's queue and Trunk does not fit it.
- **Reliability.** GitHub's queue stalls or mis-merges again in a way that costs a working day.
- **Double builds.** The PR build plus the queue build become a real capacity limit. From 2026-11-04 to 2026-11-18 PR builds used 8,462 job-minutes, queue builds 3,047, and builds of `main` 5,560, most of that scheduled.

## Recommended architecture

[opinion] In one place:

- **A landing loop in the engine** owns what happens after a merge is requested: check for conflicts before submitting, submit, watch, and on an ejection choose a tier, apply it, and resubmit. It is the only new component in the first three phases.
- **Landing backends** sit under it, one per product: GitHub's queue for mono, Trunk for flunge, and later a core of our own. The loop does not know which one it is driving beyond the capabilities the backend declares.
- **The intent store is the single authority** on whether a PR may land at a given head. The loop and any backend consume it. One rule, shared by all backends, says which engine-authored changes keep an intent alive.
- **Resolution is the existing ladder**, called by the loop: retry, then the deterministic resolvers, then a bounded model behind validators, then the existing agent revisions, then a person.
- **Only deterministic code merges.** Anything that runs a model returns a patch and holds no credentials.
- **A queue core of our own, if it is built, is a crate with ports** that the engine embeds: an optimistic chain with a window, conflict checks at admission, retry in place, and an independence oracle that starts as "nothing is independent".

Alternatives considered and set aside: a standalone service first (no users for it, and it would have to call back into the engine for everything valuable); adopting a hosted queue as the whole answer (none covers retry of our checks, lockfile regeneration and intent together); dropping the gate for detect-and-revert (a red `main` costs every open PR); and building lanes first (10% independence).

## Phased plan

| Phase | What is built                                                                                                                                                                                              | What it proves                                                                                               | Done when                                                                                                                      |
| ----- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| 0     | An ejection ledger: cause by log signature, whether the PR later landed unchanged, time to re-land. Fixes for the worst flaky tests and the `lint/rust` wall-clock limit.                                  | That the baseline in this document holds continuously, and how much of the flake rate is a few tests         | The ledger reproduces Findings 4 and 5 for a fortnight without a manual pass                                                   |
| 1     | Tier 0 in the engine: on a failed-check ejection, classify; for infrastructure and for a first test failure, re-enqueue the same head with no agent and no budget charge. A flake ledger.                  | That retry removes about half of failed-check ejections and that real failures still eject on the second try | Failed-check ejections that reach an agent fall by 40% or more; spurious re-land time falls from 21 minutes to about one build |
| 2     | Tier 1 made reliable (the conflict investigation's first two recommendations), and a conflict check before the merge is issued, against `main` and against the heads of PRs the engine has already queued. | That mechanical conflicts never reach the queue, and that the re-merge path from mono#3712 holds under load  | Lockfile-only and same-spot conflict ejections fall by 80%; 90% of lockfile-only conflicts resolve without an agent            |
| 3     | The landing-backend seam with GitHub and Trunk behind it, one intent rule for both, and tier 2 in shadow mode.                                                                                             | That the loop is backend-agnostic, and how often a bounded patch matches what an agent shipped               | Shadow agreement and validator pass rates are known for at least 50 ejections. The owner decides whether tier 2 may push.      |
| 4     | Only on a trigger: the queue core crate, run in shadow against GitHub's queue, then on one product.                                                                                                        | That the state machine is correct, and that in-place retry and speculative resolution beat re-enqueueing     | Shadow decisions match GitHub's outcomes except where ours is designed to differ; a pilot lands a week of PRs with no stop     |
| 5     | Only if a shadow measurement shows it: bazel-diff in CI and lanes with a post-merge check and automatic revert.                                                                                            | That independence is common enough to matter and sound enough to trust                                       | Independent pairs well above today's 10%; no deterministic post-merge failure attributable to lanes in a month of shadow       |
| 6     | Only on demand from PRs the engine does not manage: a GitHub App, webhooks, its own store, around the same core.                                                                                           | That the core really is separable                                                                            | A second repository lands PRs with no engine involved                                                                          |

Phases 0 to 2 are small, independent of the queue choice, and worth doing whatever is decided about the rest. Phase 3 is the last one that is clearly justified by today's data.

### Sizing

[opinion] Rough footprints per pull request against the 30-file limit on a change, taken from what a change of each shape costs in this repository: a new column and migration are four fixed files plus one per writer, a new crate is five fixed files plus two or three per consumer, and a new RPC verb is twelve to fifteen files. They are estimates to be replaced by each phase's own design doc, not commitments.

| Phase | Expected pull requests                                                                              | Rough files in each                                  | Likely to need splitting                                                              |
| ----- | --------------------------------------------------------------------------------------------------- | ---------------------------------------------------- | ------------------------------------------------------------------------------------- |
| 0     | The ledger columns and writers; one per flaky-test fix; the `lint/rust` limit                       | 8 to 12 for the ledger; 1 to 3 for a fix             | No                                                                                    |
| 1     | Classifier and re-enqueue; flake ledger storage and counters                                        | 10 to 15; 6 to 10                                    | Only if the ledger is built in the first PR, or if a new RPC verb is added to show it |
| 2     | The conflict check before the merge; the tier 1 reliability fixes                                   | 6 to 12 each                                         | Not known until the conflict diagnosis says how many fixes there are                  |
| 3     | The seam crate; the GitHub implementation; the Trunk implementation; the intent rule; tier 2 shadow | 11 to 14 for the crate; 8 to 12 for each of the rest | **Yes, the likeliest.** As one change it is well over 30 files.                       |
| 4     | The core crate and its state machine; the engine adapters                                           | 10 to 15 each                                        | Yes, the crate and its adapters should land separately                                |
| 5     | bazel-diff in CI; lanes with the post-merge check and revert                                        | 5 to 8; 15 to 25                                     | Lanes, depending on what the shadow measurement shows                                 |
| 6     | Not sized: a new binary with GitHub App adapters is several pull requests                           |                                                      | Yes                                                                                   |

The landing-backend seam is the item most likely to exceed the limit if built as one change. It replaces the hard-coded merge `match` in `review.rs`, touches both pollers and both intent stores, and normalises ejection causes across two systems, on top of the new crate's own cost. Split it along capability boundaries: the crate and normalised types with GitHub as the only implementation and no behaviour change; Trunk as the second implementation; the single intent rule; tier 2 in shadow mode as its own task. The next likeliest is phase 1: the Tier 0 re-enqueue should land before, and separately from, the flake ledger storage.

## Risks

- **Retry hides a real intermittent failure.** A PR that makes a test flaky lands on the second attempt. Mitigation: the flake ledger flags a test that first flakes in a PR that touches it; one retry only.
- **An automatic resolution is silently wrong.** Incident 002 is the precedent. Mitigation: narrow classes, the deletion tripwire, the survival check, labelled commits, post-merge sampling, per-path circuit breakers. The risk does not go to zero.
- **The deterministic tier keeps failing in practice.** The design's postmortem records four periods in which it never succeeded, mostly because "deterministic" turned out not to mean hermetic. If tier 1 is unreliable the whole loop falls through to agents and nothing improves. Phase 2 exists to settle this before anything depends on it.
- **We build for a scale that does not arrive.** Throughput in November is half of July's. The plan guards against this by gating the queue core and lanes on measured triggers.
- **We optimise the queue while PR lifetime is the real problem.** The median PR waits hours before a merge is requested, and that wait is what produces most conflicts. Queue work does not shorten it.
- **Lanes are unsound in a way we do not catch.** The hosted queues do not implement the build-graph check Uber describes, and our tests are not fully hermetic. Lanes change the guarantee on `main`.
- **A bot that can push and merge is a target.** Mitigation: an App with least privilege, no model in the merging process, validators on every model output.
- **Shared GitHub rate limit.** The engine polls on the operator's personal token and already throttles itself below 1,500 remaining calls. A queue adds calls. An App identity removes the contention.
- **Vendors ship what we are building.** Mergify's resolvers and Aviator's retry are a preview and a beta of two of our tiers. The backend seam keeps the option to adopt.
- **The queue stops when the operator's machine does.** True of merging today. It matters more once other people depend on it, which is the condition for phase 6.

## Open questions

1. **What volume is the target?** "A very large number of PRs" at 15 an hour needs none of the queue core. At 100 an hour it needs the chain and probably batching. The triggers above assume the answer is "whatever arrives"; a stated target would let phase 4 be scheduled or dropped.
2. **May a tier 2 patch keep the merge intent alive?** Today only a mechanical conflict fix re-merges on GitHub, while on Trunk an agent's fix does. This decides how much of the bounded tier's value is realised, and it should be one rule for both.
3. **Is one automatic retry acceptable as policy,** knowing it can pass a newly flaky test once?
4. **If lanes are ever built, is "`main` is green or being reverted" acceptable** in place of "`main` is always green"?
5. **Will anyone other than the engine merge PRs** in a repository we care about within the planning horizon? This is the only thing that justifies a standalone service.
6. **Should the bot be a GitHub App now,** ahead of any queue work, to separate merging from the operator's personal token?
7. **Why did the ejection rate jump in October?** Failed-check ejections went from 8% to 15% of entries between September and October, and the unchanged share from 33% to 67%. That points at a flakiness or fleet-load regression, not at the queue. Phase 0 should find it.
8. **Does Trunk admit `spinyfin/mono` into the existing Trunk organisation?** Still open from the Trunk investigation, and it decides whether Trunk is a real fallback.
9. **Is the 6% to 7% failure rate of builds on `main` known and accepted?** The scheduled builds also use more job-minutes than the queue does.

## Follow-up code changes

For separate PRs. None was made here. Rough sizes are in [Sizing](#sizing); the landing-backend trait below is the one most likely to need splitting.

- Record a cause class, the queue commit, and the eventual re-land outcome on every `merge_queue_rebounce` remediation row, and add counters for each.
- On a queue ejection classified as infrastructure, or as a first test failure, re-enqueue the same head from the engine with `enqueuePullRequest` and no revision; charge no CI budget. Give `CiLogReader::retrigger` its first production caller for the Trunk and PR-branch cases.
- Deflake or isolate `remote_wrapper::tests::wrapper_handshake_reports_the_direct_driver_pid`, `remote_wrapper::tests::wrapper_resolves_and_exports_structured_output_on_the_remote_host`, `read_admission::burst_tests::socket_saturation_with_distinct_topic_pushes_keeps_replies_and_connection` and `EngineReconnectIntegrationTests`; raise or restructure the `lint/rust` subprocess wall-clock limit.
- Before issuing a merge, run a merge-tree check of the PR against `main` and against the heads of the product's other active merge intents, and route a conflict to the mechanical rungs first.
- Introduce a landing-backend trait in the merge path with GitHub-queue and Trunk implementations, normalised entry states and ejection causes, and a single rule for which engine-authored changes keep a merge intent alive.
- Add a shadow-mode bounded fixer for compile breaks and single-file residual conflicts that records its patch and validator results next to the agent revision's outcome.
- Add a scripted version of the Finding 7 measurement (share of co-queued PR pairs with disjoint affected tests) so it can be tracked as crates are extracted.
- The two recommendations this plan depends on from the conflict investigation: diagnose against current `main`, and record why the ladder fell through.

## Appendix: reproducing the measurements

- **Queue events.** GraphQL `repository.pullRequests` with `timelineItems(itemTypes: [ADDED_TO_MERGE_QUEUE_EVENT, REMOVED_FROM_MERGE_QUEUE_EVENT])`, selecting `createdAt`, `reason` and `beforeCommit { oid }`. Pair each add with the next removal on the same PR. `beforeCommit` is the queue's squash commit and is absent on conflict removals.
- **Builds.** Buildkite REST `/pipelines/mono/builds?branch=gh-readonly-queue/main/*`. The build's commit equals the removal event's `beforeCommit`, which joins builds to ejections. Job logs come from `/pipelines/mono/builds/<n>/jobs/<id>/log`.
- **Patch comparison.** In a throwaway clone, fetch each squash commit by SHA and compare `git diff -U0 <c>^ <c>` between the ejected commit and the PR's merged commit as multisets of added and removed lines per file, ignoring `MODULE.bazel.lock` and `Cargo.lock` for the lockfile-only class.
- **Conflict replay.** The PR head at ejection is the `afterCommit` of the last force-push before the ejection, or the `beforeCommit` of the next one. `git merge-tree --write-tree --name-only` against `git rev-list -1 --first-parent --before=<ejection time> main`, with `merge.conflictStyle=diff3`; a hunk with an empty base section is a same-spot insertion.
- **Flake floor.** Builds of branch `main` in the same pipeline, split by `source` (`webhook` or `schedule`).
- **Affected tests.** `bazel query 'deps(kind(".*_test rule", //...))' --output=graph --nograph:factored --noimplicit_deps --notool_deps`, keeping first-party edges, then the mapping described under Methods.
- **Engine ledgers.** `boss engine ci list --json --limit 0` and `boss cost top --since <date> --limit 5000 --json`, joined on the remediation's revision task.
