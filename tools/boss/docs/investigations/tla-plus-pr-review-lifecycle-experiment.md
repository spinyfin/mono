# Does a TLA+ model of the `pr_review` lifecycle earn its keep?

- **Date:** 2026-07-24
- **Kind:** methodology experiment (no engine code changed)
- **Verified against:** `spinyfin/mono` `origin/main` = `d7c26f6c9c72cd6181138724702eb7fda586f0a0`
- **Test case:** [PR #2331](https://github.com/spinyfin/mono/pull/2331) — open, **not merged**, so `main` still has the bug
- **Artifacts:** [`tla/PrReviewLifecycle.tla`](tla/PrReviewLifecycle.tla) + ten `.cfg` model configs
- **Tooling:** TLC 2.19 (`tla2tools.jar`, downloaded — not vendored in this repo)

The question was narrow and falsifiable: would a TLA+ spec of the `pr_review` execution lifecycle, written from `main`, have caught the churn-guard scoping bug that PR #2331 diagnoses by hand? TLC did find it, in about three seconds, from a spec that took roughly 15 minutes to write. But the finding turns almost entirely on one judgement call made _before_ TLC ran, and that is the real result of this experiment.

## Verdict

**Qualified no — not worth doing again for this class of change.**

The model checker did its job. The problem is that the decisive step was writing an invariant that states _what the guard is for_, and once you have written that sentence down you have already found the bug — TLC only confirms it. The guard's own code comment (`pr_review_recovery.rs:134-142`) explicitly describes the buggy behaviour as intentional ("terminal executions **of any kind**"), so a spec author faithfully transcribing `main` would encode the defect into the invariant as well as the action, and TLC would report success.

The sharpest evidence for that verdict is what PR #2331 already contains. Both of the durable artifacts this exercise identified — a test that `cancelled` counts toward the guard, and a test that unrelated implementation churn does not trip it — are **already in that PR**, written by hand, with no TLA+ involved. The model reproduced two defects that were already diagnosed and already fixed.

## Ground truth, re-verified

The task description pinned `916d4631`. `main` has since moved to `d7c26f6c`, and `execution_kind_for_work_item` **relocated** from `work/dispatch_helpers.rs` to `work/exec_status_helpers.rs:3-31` in between. Everything else load-bearing still holds at the current sha:

| Claim                                                         | Location on `d7c26f6c`                             | Status                     |
| ------------------------------------------------------------- | -------------------------------------------------- | -------------------------- |
| `is_terminal()` = 5 of 12 statuses                            | `protocol/src/types/execution.rs:126-131`          | confirmed                  |
| `is_live()` = `Running \| WaitingHuman` only                  | `protocol/src/types/execution.rs:133-135`          | confirmed                  |
| Guard counts **any kind**, `cancelled` absent                 | `engine/core/src/work/dispatch.rs:830-846`         | confirmed                  |
| Candidate query is kind-scoped, `cancelled` present           | `engine/core/src/work/dispatch.rs:789-824`         | confirmed                  |
| Window = 3600s, threshold = 3                                 | `engine/core/src/work.rs:44,48`                    | confirmed                  |
| `execution_kind_for_work_item` can never mint `PrReview`      | `engine/core/src/work/exec_status_helpers.rs:3-31` | confirmed (**moved file**) |
| `request_resume_execution` copies `dead.kind` verbatim        | `engine/core/src/work/executions_runs.rs:184-240`  | confirmed                  |
| `mark_execution_orphaned` bails if already terminal (the CAS) | `engine/core/src/work/executions_runs.rs:97-102`   | confirmed                  |
| SlotBusy `PrReview` carve-outs                                | `engine/core/src/coordinator.rs:6857`, `:7029`     | confirmed                  |
| Sweeps are unsynchronised, fire-immediately-then-sleep        | `engine/core/src/sweep_loop.rs` `spawn_sweep_loop` | confirmed                  |

One claim I could not confirm as stated: the description calls the missing `cancelled` and the missing kind filter "two independent defects" of `count_recent_terminal_executions`. They are, but there is a **third** property of that query nobody wrote down — see Finding 3.

## Method, and the discipline problem

The experiment as specified asks for a spec written "blind to the fix." That is not achievable here and I am not going to claim it was: I read PR #2331's diagnosis before writing a line of TLA+. Pretending otherwise would make the whole writeup worthless.

What I did instead was make the contamination the object of study. The spec models the lifecycle faithfully and generically — statuses, the kind asymmetry, eight death transitions, the sweep — and does **not** encode PR #2331's `Option<ExecutionKind>` parameter. The invariants are stated in terms of what `pr_review_recovery` exists to do, not in terms of the patch. Then I report explicitly which invariants a genuinely blind author would plausibly have written. That distinction is where all the signal is.

The one modelling decision that mattered most: `pr_review_recovery::run_one_pass` is split into **three separate TLA+ actions** (`RecoveryScan` → `RecoveryCount` → `RecoveryPark`/`RecoveryRefire`) with a program counter. Every `WorkDb` call is individually atomic (one sqlite connection behind a `std::sync::Mutex`), but a sweep pass is a _sequence_ of such calls with no enclosing transaction. A model that treats a pass as atomic cannot express the states that make this subsystem interesting.

## Results

Ten TLC configurations. `MaxExecs`/`MaxImpls`/`MaxTime`/`MaxDeaths` are state-space bounds, not engine constants.

| Config                    | Question                                                           | Verdict                                                                                       | Wall time             |
| ------------------------- | ------------------------------------------------------------------ | --------------------------------------------------------------------------------------------- | --------------------- |
| `StatusSetConsistency`    | Do the guard's and the candidate query's "dead" status sets agree? | **FALSE in the initial state**                                                                | 1s, 0 states explored |
| `PrReviewLifecycle`       | Does the guard park a review whose own history is clean?           | **`GuardParksOnlyOnReviewChurn` violated**, 16-state trace                                    | 3s                    |
| `ControlBaseline`         | Same, at the control's bounds                                      | **violated**                                                                                  | 1s                    |
| `Control`                 | Same, but with the count kind-scoped                               | **exhaustive, no error** (419,792 distinct states, 0 left on queue)                           | 6s                    |
| `CancelledBypass`         | All deaths are `cancelled` — does anything catch it?               | **no error** — both properties hold; the model is blind to it (Finding 2)                     | 1s                    |
| `MidPassMutation`         | Can another sweep mutate `work_executions` mid-pass?               | **violated**, 9-state trace (a deliberate probe — Finding 6)                                  | 1s                    |
| `Liveness`                | Does an open PR whose review dies eventually get a `ReviewResult`? | **violated**, `Scan→Count→Park` lasso                                                         | 14s                   |
| `TransientResume`         | Safety, with the unguarded resume path enabled                     | **violated**                                                                                  | 2s                    |
| `TransientResumeLiveness` | Does `transient_recovery`'s unguarded resume path rescue it?       | **violated**, same lasso                                                                      | 43s                   |
| `Drain`                   | Is the park permanent, or a stall bounded by the window?           | violated, but the counterexample is a **bound artifact** — question **unresolved**, Finding 4 | 290s, 4.8M states     |

`ControlBaseline` and `Control` are a **matched pair** at identical bounds. Without that pair, "the fix passes" would prove nothing — a bound too small to reach the bug at all would also pass, and would look exactly like the fix working.

### Reproducing

TLC is not vendored in this repo and nothing in the build depends on it. To re-run:

```sh
curl -sL -o /tmp/tla2tools.jar \
  https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar

# Run from a scratch dir: TLC writes a multi-GB `states/` cache
# into its working directory, which must not land in the repo.
mkdir -p /tmp/tlc && cd /tmp/tlc
cp <mono>/tools/boss/docs/investigations/tla/* .

java -XX:+UseParallelGC -cp /tmp/tla2tools.jar tlc2.TLC \
  -config PrReviewLifecycle.cfg PrReviewLifecycle.tla
```

All ten configs were re-run against the exact committed artifacts on TLC 2.19; every verdict in the table above is from that final run.

## Finding 1: TLC reproduces the churn-guard bug

The baseline run violates `GuardParksOnlyOnReviewChurn`, which says: _if the guard has parked the item, the item's own **review** history must actually be churning._

```tla
GuardParksOnlyOnReviewChurn ==
    parked => CountRecentTerminalReviews >= ChurnThreshold
```

The minimal counterexample is 16 states. Abbreviated to the transitions that matter (state numbering is TLC's; the full verbatim trace is at the end of this section):

```
State  2-4 : DispatchImpl -> StartExec -> Die     impl #1 -> "orphaned"
State  5-7 : DispatchImpl -> StartExec -> Die     impl #2 -> "orphaned"
State  8-9 : DispatchImpl -> StartExec            impl #3
State   10 : ImplCompletes                        prOpen = TRUE
State   11 : RequestFirstReview                   pr_review #4 created "ready"
State   12 : StartExec                            pr_review #4 -> "running"
State   13 : Die                                  pr_review #4 -> "orphaned" (deaths = 3)
State   14 : RecoveryScan                         sweep = [pc |-> "scanned", cand |-> 4, cnt |-> 0]
State   15 : RecoveryCount                        sweep = [pc |-> "acting",  cand |-> 4, cnt |-> 3]
State   16 : RecoveryPark                         parked = TRUE
```

Final state, verbatim from TLC:

```
State 16: <RecoveryPark line 264, col 5 to line 268, col 51 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 3
/\ sweep = [pc |-> "idle", cand |-> 4, cnt |-> 3, nExecs |-> 4]
/\ prOpen = TRUE
/\ parked = TRUE
```

`cnt = 3` is made of two `impl` failures plus the review's _first_ death. The review's own dead-review count is **1**. Threshold is 3. This is exactly PR #2331's diagnosis, arrived at mechanically.

<details>
<summary>Full verbatim TLC counterexample (16 states)</summary>

```
Error: Invariant GuardParksOnlyOnReviewChurn is violated.
Error: The behavior up to this point is:
State 1: <Initial predicate>
/\ execs = <<>>
/\ clock = 0
/\ deaths = 0
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 2: <DispatchImpl line 184, col 5 to line 188, col 59 of module PrReviewLifecycle>
/\ execs = <<[created |-> 0, status |-> "ready", kind |-> "impl"]>>
/\ clock = 0
/\ deaths = 0
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 3: <StartExec line 191, col 5 to line 194, col 63 of module PrReviewLifecycle>
/\ execs = <<[created |-> 0, status |-> "running", kind |-> "impl"]>>
/\ clock = 0
/\ deaths = 0
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 4: <Die line 201, col 5 to line 206, col 55 of module PrReviewLifecycle>
/\ execs = <<[created |-> 0, status |-> "orphaned", kind |-> "impl"]>>
/\ clock = 0
/\ deaths = 1
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 5: <DispatchImpl line 184, col 5 to line 188, col 59 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "ready", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 1
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 6: <StartExec line 191, col 5 to line 194, col 63 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "running", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 1
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 7: <Die line 201, col 5 to line 206, col 55 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 8: <DispatchImpl line 184, col 5 to line 188, col 59 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "ready", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 9: <StartExec line 191, col 5 to line 194, col 63 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "running", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = FALSE
/\ parked = FALSE

State 10: <ImplCompletes line 209, col 5 to line 214, col 55 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = TRUE
/\ parked = FALSE

State 11: <RequestFirstReview line 224, col 5 to line 229, col 59 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "ready", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = TRUE
/\ parked = FALSE

State 12: <StartExec line 191, col 5 to line 194, col 63 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "running", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 2
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = TRUE
/\ parked = FALSE

State 13: <Die line 201, col 5 to line 206, col 55 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 3
/\ sweep = [pc |-> "idle", cand |-> 0, cnt |-> 0, nExecs |-> 0]
/\ prOpen = TRUE
/\ parked = FALSE

State 14: <RecoveryScan line 252, col 5 to line 256, col 59 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 3
/\ sweep = [pc |-> "scanned", cand |-> 4, cnt |-> 0, nExecs |-> 4]
/\ prOpen = TRUE
/\ parked = FALSE

State 15: <RecoveryCount line 259, col 5 to line 261, col 59 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 3
/\ sweep = [pc |-> "acting", cand |-> 4, cnt |-> 3, nExecs |-> 4]
/\ prOpen = TRUE
/\ parked = FALSE

State 16: <RecoveryPark line 264, col 5 to line 268, col 51 of module PrReviewLifecycle>
/\ execs = << [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "impl"],
   [created |-> 0, status |-> "completed", kind |-> "impl"],
   [created |-> 0, status |-> "orphaned", kind |-> "pr_review"] >>
/\ clock = 0
/\ deaths = 3
/\ sweep = [pc |-> "idle", cand |-> 4, cnt |-> 3, nExecs |-> 4]
/\ prOpen = TRUE
/\ parked = TRUE
```

</details>

The `Control` run — same spec, same bounds, count restricted to `pr_review` rows — explores the state space exhaustively and finds no error. So the invariant discriminates between the two implementations rather than being unsatisfiable.

## Finding 2: the `cancelled` mismatch needs no model checker at all

`GuardAndCandidateAgreeOnDeath` is a set equality between two constants transcribed from adjacent functions:

```tla
GuardCountedStatuses == { "orphaned", "abandoned", "failed" }              \* dispatch.rs:834
DeadReviewStatuses   == { "orphaned", "abandoned", "failed", "cancelled" } \* dispatch.rs:799
```

TLC's response:

```
Error: The invariant of GuardAndCandidateAgreeOnDeath is equal to FALSE
```

It reports this **before computing a single reachable state** — it never runs the behavioural search. A review that dies `cancelled` is a recovery candidate but is invisible to the guard meant to rate-limit recovering it.

This is worth dwelling on, because it cuts hard against the method being evaluated. The second defect in this subsystem is caught by _writing the two status sets next to each other_. The temporal-logic machinery, the interleaving model, the hundreds-of-thousands-of-states searches — none of it contributed. A three-line assertion in any language would do.

It is worse than that, though. I built `CancelledBypass` to check whether the behavioural model catches this defect _as behaviour_ — every reaper writes `cancelled`, so the guard's count stays at 0 forever while reviews keep dying and refiring. TLC's answer:

```
Model checking completed. No error has been found.
```

Both `GuardParksOnlyOnReviewChurn` and `ReviewEventuallyLands` **hold**. The safety property holds vacuously (a guard that never fires never fires wrongly), and liveness holds because with deaths bounded the review does eventually complete — the missing rate limit costs extra refires, not correctness. Demonstrating the unbounded-churn consequence would need unbounded deaths, which finite-state model checking cannot express.

So the behavioural model is **blind** to a defect that a one-line set comparison catches immediately. That is a pointed result: the expensive part of the method missed what the cheap part found.

## Finding 3: the window is keyed on `created_at`, not on death time

Not in PR #2331's body, not in the guard's comment, and it survives the proposed fix. Both churn queries filter:

```sql
AND CAST(created_at AS INTEGER) >= ?2
```

(`work/dispatch.rs:834` and `:853`.) `created_at` is epoch-seconds-as-string (`work/audit_misc.rs:76-78`), so the `CAST` is sound — I checked that first, since an ISO-8601 string would have made the comparison always-false and the guard dead code. It is not.

But the predicate means the guard counts _executions **started** in the last hour that later died_, not _executions that died in the last hour_. Consequences, both directions:

- A long-running review created 61 minutes ago that dies right now is **invisible** to the guard, however many times it happens.
- A burst of short-lived executions created and killed inside one minute all count, even if the item has been healthy for the preceding 59.

I found this by transcribing the SQL to write `InWindow`, not from any TLC output. It is a product of the discipline of restating code in another notation — which is a real benefit of spec-writing, but is not model checking.

## Finding 4: liveness fails, and the "permanent" framing needs a caveat

`ReviewEventuallyLands == prOpen ~> ReviewDone` is violated. `ReviewDone` means a `pr_review` reached `completed`, which is exactly "a `ReviewResult` was produced" — `finalize_pr_review_pass` is the only path to that status (`dispatch.rs:775-780`). The counterexample terminates in a **lasso**:

```
State 15: <RecoveryScan ...>
State 16: <RecoveryCount ...>
State 17: <RecoveryPark ...>
State 18: <RecoveryScan ...>
State 19: <RecoveryCount ...>
Back to state 17: <RecoveryPark ...>
```

The sweep runs forever, re-scans the same dead review, recounts 3, re-parks, and never refires. The PR is never reviewed. That is a stronger statement than "it parks once": the model shows a stable non-progressing cycle.

I could **not** confirm the PR body's stronger claim that this parks auto-retry _permanently_. The guard self-heals once terminal executions age out of the window (`work/dispatch_helpers.rs:1362`, `:1374`), so in principle this is a stall bounded by the churn window rather than a permanent park, unless the item keeps producing terminal executions to refill it. My `Drain` config — window (1) shorter than the horizon (4), so rows should age out — **failed to settle the question**, and the way it failed is the most transferable lesson in this writeup.

`ReviewEventuallyLands` is reported violated after 290 seconds and 4.8M distinct states. But the counterexample is worthless. Its final state:

```
/\ clock = 4
/\ deaths = 3
/\ sweep = [pc |-> "acting", cand |-> 4, cnt |-> 3, nExecs |-> 4]
/\ prOpen = TRUE
/\ parked = TRUE

Back to state 20: <RecoveryPark ...>
```

The trace reaches `clock = 4`, which _is_ `MaxTime`, with the counted rows still inside the window. `Tick` requires `clock < MaxTime`, so the clock is frozen, nothing can ever leave the window, and the lasso is a property of the bound rather than of the engine. I first hit this at `MaxTime = 2` and assumed it was too small; raising it to 4 reproduced the identical shape one tick higher, at roughly 100× the states.

That is the general result, and it will recur for anyone trying this: **a finite clock cannot answer "does this self-heal after a time window," because TLC's adversary will always schedule the churn at the last representable instant.** Raising the ceiling does not help — it relocates the counterexample. Answering the question properly needs a different encoding (an explicit `AgeOut` action decoupled from a global clock, or an unbounded clock, which finite-state checking cannot have).

So: confirmed that it parks, and that the park is a stable non-progressing cycle within the modelled horizon. _Not_ confirmed that it is permanent in the real system — and this spec, as written, structurally cannot confirm or refute it. The honest reading is that the PR body may overstate the duration while being right about the mechanism.

## Finding 5: `transient_recovery` does not rescue the property

`transient_recovery.rs:444` calls `request_resume_execution`, which copies `dead.kind` verbatim (`work/executions_runs.rs:184-240`) — so it can mint a `pr_review` successor **without ever consulting the churn guard**. Two recovery paths for the same failure class with different guard policies is a smell worth probing.

With that path enabled, liveness is _still_ violated, with the same lasso. The reason is structural: `transient_recovery` only covers deaths it classifies as transient, and the SlotBusy pane-spawn `failed` path is explicitly not one — `PrReview` is carved out of demote-to-todo (`coordinator.rs:6857`) and out of the inline `request_execution` refire (`coordinator.rs:7029`). For that death class `pr_review_recovery` really is the only way back, and it is the gated one.

## Finding 6: an interleaving a serialized model would not show

The `MidPassMutation` config asserts the sweep pass is atomic (`NoMutationMidPass`), which it plainly is not — there is no transaction around `run_one_pass`'s sequence of `WorkDb` calls. TLC exhibits the window in nine states:

```
State 8: <RecoveryScan>
/\ sweep = [pc |-> "scanned", cand |-> 2, cnt |-> 0, nExecs |-> 2]

State 9: <DispatchImpl>
/\ sweep = [pc |-> "scanned", cand |-> 2, cnt |-> 0, nExecs |-> 2]
```

A new `work_executions` row appears while the sweep sits at `pc = "scanned"` — between `list_dead_pr_review_candidates` and `count_recent_terminal_executions`. The count the guard acts on is therefore **not** a count of the state that produced the candidate list. This is a genuine TOCTOU that an atomic-pass model cannot express, and it is the one finding here specifically attributable to the concurrency modelling.

In fairness to the method: I only got this trace by writing a _deliberate_ probe invariant after noticing the interleaving in an earlier run. Once the model bounds were tightened, TLC's minimal counterexample for the real properties no longer exhibited it — shortest-trace search actively hides incidental interleavings. Finding them takes a targeted assertion, which means knowing to look.

Its practical weight is limited, though, and I will not oversell it: it compounds an already-over-counting guard rather than being independently harmful. Note also that repeat trips are folded rather than spammed (`work/tests/t29.rs:356`), so the cycle in Finding 4 does not flood the operator with attention items. The model raised that question; the existing tests answered it.

## What the model could not answer

PR #2331's two open questions are both outside what a specification can decide:

- **"Is this the dominant cause among orphaned/failed reviews?"** — unanswerable in principle here. Model checking establishes _reachability_, never _frequency_. TLC says this trace exists; it says nothing about how often production takes it. That needs telemetry.
- **"The fix causes more refires, so the raw completion ratio can legitimately look worse."** — a metric-design question. The spec has nothing to say about it.

Also unanswered: **can a review be starved by pool routing?** The spec models a single work item with no pools, slots, or claims. Answering that would need cross-item contention modelled, which is a substantially larger spec — plausibly the one place where the concurrency machinery would genuinely pay for itself.

## Where the abstraction lies

Stated plainly, because these are the places a bug could hide from the model:

- **11 execution kinds collapse to 2** (`impl`, `pr_review`). Safe for this question only because `execution_kind_for_work_item` provably cannot mint `PrReview`; if that ever changes, the abstraction silently becomes wrong.
- **No sqlite, no tokio, no `gh` latency.** Interleaving is modelled at `WorkDb`-call granularity on the assumption each call is atomic. That assumption is load-bearing and comes from `work.rs:385` (one connection, one mutex) — the model does not verify it, it _assumes_ it.
- **One work item, no pools.** Excludes an entire class of starvation bug.
- **The CAS is approximated.** `mark_execution_orphaned` bails when already terminal; I model this as "a death requires status `running`". Same net effect (one reaper wins) but I never model two reapers racing explicitly.
- **Time is a small bounded integer** standing in for a 3600-second window. Finding 4 shows this is not a harmless simplification — it is the elision that cost the most and delivered the least. The clock ceiling does not merely limit what can be checked; it actively fabricates counterexamples that look like engine bugs.
- **`MaxExecs`/`MaxImpls` are pure artifacts.** I got a false liveness violation from them: a trace where impl churn exhausted the execution budget, no `pr_review` was ever created, and the behaviour simply stuttered. It looked like a real bug. I only caught it by reading the trace's final state and noticing no review existed. Splitting the budget (`MaxImpls`) fixed it. **A bounded model produces counterexamples that are about the bound, and nothing warns you.**

## Is the spec maintainable?

No. It is write-once.

The spec cites roughly 25 specific line numbers across a dozen files. Between the sha this task pinned (`916d4631`) and the sha I verified against (`d7c26f6c`) — days apart — `execution_kind_for_work_item` moved to a different file. Every one of those citations is a rot vector, and nothing in CI checks them. There is no owner: the person who next changes `stale_worker_sweep` will not know this file exists, and if they did, updating it requires fluency in TLA+ that the repo does not otherwise demand.

A spec that silently drifts from the code is worse than no spec, because it invites the reader to trust conclusions that no longer follow from anything.

## Scoring the method

**Cost.** About 95 minutes end to end: ~10 verifying ground truth against the current sha, ~15 writing the spec, and ~70 running TLC and fighting model bounds. That last number is the honest one and it dominates. The spec was the cheap part; making the _results_ trustworthy was not. Three of the ten configs produced output that was an artifact rather than a finding — one false liveness violation from an exhausted execution budget, and two from the clock ceiling — and each had to be diagnosed by reading traces state by state.

I cannot measure what the PR author spent on the hand reconstruction; the observable artifact is a 901-word diagnosis attached to an 8-file, +200/−35 change. What I can say is that the hand analysis produced a correct, actionable diagnosis with no false leads, and the model produced a correct diagnosis plus three false leads I had to spend most of my time clearing.

**What the model caught that prose didn't.** One thing, honestly: the mid-pass interleaving (Finding 6), which is real and would be hard to see by reading — and even that needed a probe invariant written specifically to expose it. Finding 3 came from transcription discipline, not from TLC. Finding 2 came from putting two constants side by side, and `CancelledBypass` showed the behavioural model actively _missing_ it. Finding 5 confirms something the hand analysis already implied.

**What prose caught that the model didn't.** The important part. The chain of carve-out reasoning that establishes _`pr_review_recovery` is the only recovery path for a SlotBusy-killed review_ — `coordinator.rs:6857`, `:7029`, `dispatch.rs:742-754` — is what makes the bug severe rather than cosmetic. The model did not derive that. I **fed** it to the model as a modelling assumption (`RequestFirstReview` fires once). The hard intellectual work was code reading, and the spec consumed its output.

That is the crux. The spec was downstream of the analysis, not a substitute for it.

**Would a blind author have found it?** Only by writing `GuardParksOnlyOnReviewChurn` — an invariant about the guard's _purpose_. The guard's own comment says it counts terminal executions "of any kind" and describes that as deliberate. An author transcribing `main` faithfully would encode `any kind` into the invariant too, and TLC would report no error. The bug is found by asking "what is this guard _for_?", and that question is free — it needs no tooling.

## Recommendation

**Do not repeat this for bug-hunting in existing sweep code.** The cost is real, the artifact rots, and the decisive insight was available without it.

**It is plausibly worth it for one narrow class:** designing a _new_ coordination protocol with genuinely unknown interleavings and no ground truth — multi-writer claim protocols, a distributed scheduler, cross-host lease handoff. There, the "what would I even assert?" question is the deliverable and there is no existing code to read instead. The single-item, single-mutex sweep audited here is not that.

If a spec is ever written for keeps, it needs an owner and a CI check that the cited line numbers still resolve. Without both, do not write it.

The durable value from this exercise would have been two assertions rather than a spec — and PR #2331 already ships both of them (`churn_guard_counts_cancelled_reviews` and `unrelated_implementation_churn_does_not_trip_review_churn_guard` in `work/tests/t29.rs`), written by hand. That is the cleanest possible statement of the result: the hand method produced the durable artifact; the formal method produced a writeup.

## Follow-ups (out of scope for this PR — no engine code was changed)

I checked PR #2331's diff before writing these. It already fixes the `cancelled` mismatch (`status IN (... , 'cancelled')` in both churn queries) and already adds both regression tests this exercise would have recommended, so **Findings 1 and 2 need no follow-up** — they are fixed and covered.

Two findings survive that PR, because it does not touch either:

1. **Decide whether the churn window should key on death time rather than `created_at`** (Finding 3). PR #2331 renumbers the bind parameter but keeps `CAST(created_at AS INTEGER) >= ?`, so the guard still measures "executions _started_ in the window that later died", not "deaths in the window". A review created 61 minutes ago that dies now is invisible to it. That may be the intended semantics — it is currently written down nowhere, and it is a behaviour question for a human rather than a bug to fix unilaterally.

2. **Reconcile the two recovery paths' guard policies** (Finding 5). `transient_recovery.rs` is not in PR #2331's changed files. Its `request_resume_execution` path copies `dead.kind` verbatim and re-fires a `pr_review` **without consulting the churn guard at all**, while `pr_review_recovery`'s refire is gated by it. Two recovery paths for overlapping failure classes with opposite rate-limiting policies is worth an explicit decision, even if the answer is "intentional".
