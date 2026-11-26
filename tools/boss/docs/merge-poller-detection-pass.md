# Merge poller detection pass

The merge poller runs one full detection pass a minute. A pass lists every
work item with a PR, probes those PRs (in bounded chunks, at the depth each
one needs; see [Probe tiers](#probe-tiers-and-chunks)), records what the probe
says, and then runs the upkeep that rides the same cadence.
This page covers the order a pass works in, what happens when it runs out
of time, and how to find out where its time went.

Source: `tools/boss/engine/merge-poller/src/pass.rs` (the pass),
`walk_order.rs` (the rotation), `pass_trace.rs` (timing and the health
signal), `schedule.rs` (the loop and the budget), `probe_tier.rs` (which PRs
get which probe) and `probe.rs` (the chunked GraphQL probe).

## Order of a pass

A pass has a hard budget of three cadences (180 s at the 60 s cadence). It
is ordered so that what must not be missed comes first and what can be slow
comes last.

| Phase                                                         | What it does                                                                            | Cost scales with                    |
| ------------------------------------------------------------- | --------------------------------------------------------------------------------------- | ----------------------------------- |
| `list_candidates`                                             | The candidate-list queries                                                              | Local DB                            |
| `merge_disarm`                                                | Turns off auto-merge for merges conflict resolution stopped                             | Pending disarms, usually zero       |
| `probe`                                                       | Full probe of the PRs the poller acts on, then a shallow probe of those due             | Chunks of 25 / 100 PRs              |
| `terminal_walk`                                               | Records every PR the probe reports merged or closed                                     | PRs that just merged or closed      |
| `open_walk`                                                   | Upkeep of the PRs probed in full that are still open (CI, conflicts, merge-queue state) | Full-tier open PRs, local DB        |
| `stranded_ci`, `stranded_blocked`                             | Recovers remediation that lost its worker or its signal                                 | Usually zero items                  |
| `queue_rebounce`                                              | One batched merge-queue dequeue query, then per-PR checks                               | One round trip                      |
| `stalled_reviewer`, `review_admission`, `review_batch_upkeep` | Reviewer fallback and review-pool upkeep                                                | Local DB                            |
| `pr_recheck`, `background_nudges`, `late_pr`                  | Per-execution PR detection for workers with no PR bound yet                             | Live workers, GitHub calls per item |

Two properties follow from that order:

- **A merge is recorded before any open-PR upkeep.** Observing a merge costs
  the list queries, the probe and that PR's own transition. It does not wait
  behind the upkeep of every PR listed ahead of it.
- **Open walk, local upkeep and per-execution families rotate.** Terminal
  observation always stays first. The other three groups take turns starting
  after it, so a slow open walk or slow upkeep cannot starve recovery
  forever. The open walk also stops when a quarter of the pass budget
  remains, and does not stamp the candidates it did not start. Starting an
  item with less than that left would be dropped by the outer timeout and
  starve the tail. The in-pass clock expires a small margin before the
  outer timeout so a group can yield instead of being dropped.
  The three per-execution families (PR recheck, background nudges, late PR)
  rotate which one starts, and each family's candidates are ordered
  least-recently-attended first. The six local upkeep subphases (stranded
  CI rescue, stranded-blocked reconcile, queue rebounce, stalled reviewer,
  review admission, review batch upkeep) also rotate which one starts, and
  the stranded and stalled-reviewer item lists are ordered
  least-recently-attended first, so a slow early subphase or a long list
  cannot keep the later work from running on every pass.

## Probe tiers and chunks

A full probe (CI rollup, reviews, mergeability, merge-queue state) is about 51
requested GraphQL nodes per PR, and it is only worth that cost for a PR the
poller will act on. Every other tracked PR is watched only so that a merge or a
close is still recorded, and a shallow probe answers that for a handful of
scalar fields.

| Tier    | Rows                                                                                                                                             | Probed                          |
| ------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------- |
| full    | blocked rows (merge conflict, CI failure, dependency or review feedback, stranded)                                                               | every pass                      |
|         | `in_review` rows in an active product that are not parked and not dormant                                                                        | every pass                      |
|         | `in_review` rows with a merge in flight (an active merge request, durable `tasks.merge_queue_state`, or last seen queued), including parked rows | every pass                      |
| shallow | parked rows without a merge in flight, `in_review` rows in other products, rows GitHub has not updated for 14 days                               | when not observed for 5 minutes |
|         | rows watched only for merge or close: a row held `active` for a review, and revisions in review whose root is terminal or deleted                | when not observed for 5 minutes |

Merge and close detection covers every tracked PR in every product; only how
often a PR is asked about, and how much, depends on its tier. A row that is
due for a shallow probe and was never observed (an engine start, a row that has
just appeared) is probed on the next pass. A shallow probe that fails leaves its
rows due, so they are asked again next pass.

- **Active product** is the product the Boss app has selected, plus every
  product named by dispatch focus, directly or through a focused project
  (`PollerActiveProducts` in `engine/app/src/app/selected_product.rs`). When
  none is singled out (no app connected, nothing selected, focus off) every
  product is active, so the tiers change nothing.
- **Dormant** is read from GitHub's `updatedAt` at the last probe of either
  tier. A shallow probe reads it again, so a pushed or commented PR is back in
  the full tier on the next pass. A PR the poller has not probed since the
  engine started is not dormant.
- **A shallow result for an open PR is not acted on.** It carries no CI,
  review or mergeability state, so the open walk, merge-queue rebounce and every
  other open-PR upkeep run only for PRs probed in full. A shallow result for a
  merged or closed PR goes through the same terminal walk as a full one: it
  carries the merge commit, the head and base refs and the repository default
  branch, so a merge into a non-default branch is still refused.
- **Nothing remembers "not merged".** The poller remembers when a PR was last
  observed and what `updatedAt` it showed, never that it was unmerged; a row
  that is not due is simply not asked about this pass.

## Rows watched only for merge or close

The lists a pass acts on are keyed on the status of the row that owns the PR:
`in_review`, and `blocked` for a reason the poller remediates. Two states are
on none of them, and their PR is watched so that a merge or a close is still
recorded. Neither gets any open-PR upkeep.

| Watched                                                                                           | Listed by                                 | On a merge or a close                                                                                                             |
| ------------------------------------------------------------------------------------------------- | ----------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| A PR-owning row held `active` with its PR bound and no unfinished run that could still push to it | `list_chores_held_active_with_pr`         | The same transition an `in_review` row takes: `done`, with its revisions settled and its pending reviews cancelled (see below)    |
| Revisions resting `in_review` whose chain root is `done`, `archived`, deleted or missing          | `list_orphaned_in_review_revision_chains` | Each revision moves to `done`; the status ledger records `settle_orphaned_revisions_pr_merged` or `..._pr_closed_unmerged` for it |

The first is the review hold. A row whose implementation finished stays
`active` until the automated review of its PR decides, and the review can be
held for as long as dispatch focus, an all-spawns pause or a full review pool
lasts. The deferred-admission sweep does not read the PR of a held row, so
without this watch a PR merged during the hold left the row `active` for
good. Review work that is queued, running or held never keeps the row off the
list; an unfinished run of a kind that writes the PR branch does, because
that run's own completion reads the PR's state.

When the PR ends, the pass retires every review still pending for it, with the
merge or close as the recorded reason: queued review guides, single-reviewer
`pr_review` executions (the pipeline used while review-batch fan-out is off)
of the row and its revisions, and live pre-merge review batches. A batch is
failed with the abandon cause `review_batch_pr_ended` and files no
`pr_review_batch_stale` attention, since a merge during a review is not
something a human has to decide. Post-merge review batches are left alone.

The second exists because a revision is settled by its root's merged or
closed transition. A root moved to `done` or `archived` by hand, or deleted,
is not probed, so that transition never ran for its chain. A merge into a
branch other than the default is recorded for these revisions as a close: the
work did not land, and no root is left to carry a base refusal.

Review guides still queued for the PR are cancelled with the reason `parent
PR merged` or `PR closed unmerged`. A pre-merge review batch whose root has
just become `done` is failed by the review-batch upkeep that follows the
terminal walk, which retires its unfinished reviewers under the abandon cause
`review_batch_reaped`. The row's open `pr_review_admission_deferred` item is
resolved in the transition. Follow-up proposals and every other attention
item are left open.

Each tier's probe is split into fixed-size chunks, one GraphQL query per chunk,
sent one at a time in attendance order (previously unfinished and known-terminal
PRs first):

| Probe   | PRs per query | Measured on the poller's repository (`gh api graphql`)                                 |
| ------- | ------------: | -------------------------------------------------------------------------------------- |
| full    |            25 | 1,275 nodes, 1 point, 2.5-3.2 s (125 PRs in one query: 6,375 nodes, 5 points, 5.5-6 s) |
| shallow |           100 | 0 nodes, 1 point, 0.8 s (200 PRs: 1.05 s)                                              |

Each chunk gets its own deadline, `min(30 s, a quarter of the pass time left)`
taken when the chunk starts, and the bisect fallback runs inside the chunk.
A chunk that GitHub rejects or that never returns costs its own PRs only: the
chunks after it are still sent. A chunk reached with its allowance already
spent is deferred, and its PRs sort to the front of the next pass. A rate-limit
rejection stops the rest of the call.

The snapshot the walks write from ages from when the oldest contributing
chunk started, not from when the last chunk finished. If later chunks run
long enough that the first results are more than 60 s old, the open-PR walk
re-probes the remaining PRs before acting.

A PR in GitHub's merge queue, or with auto-merge armed, is probed in full
every pass even after an engine restart: the pass unions live
`tasks.merge_queue_state` with recorded merge requests. A shallow probe also
reads `mergeQueueEntry` and `autoMergeRequest`, so an external enqueue of a
row that had gone shallow is noticed and the next pass promotes it.

Every pass logs one `merge poller: probe tiers` line at `info`:

| Field                                                                                                              | Meaning                                                                       |
| ------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------- |
| `full_prs`, `full_observed`                                                                                        | PRs probed in full and how many answered                                      |
| `full_chunks_attempted`, `_failed`, `_deferred`                                                                    | Chunks sent, chunks that left a PR unobserved, chunks never sent              |
| `full_oldest_unobserved_secs`                                                                                      | Longest any full-tier PR has gone without an answer                           |
| `shallow_prs`, `shallow_due`, `shallow_observed`                                                                   | Shallow-tier PRs, those due this pass, those that answered                    |
| `shallow_chunks_*`, `shallow_oldest_unobserved_secs`                                                               | The same for the shallow tier; this one growing past ~5 minutes is starvation |
| `rows_blocked`, `rows_active_product`, `rows_merge_in_flight`, `rows_parked`, `rows_other_product`, `rows_dormant` | Why each row is in its tier                                                   |
| `rows_terminal_watch`                                                                                              | Rows and chains watched only for merge or close                               |
| `active_products`, `every_product_active`                                                                          | How many products are active, or that none was singled out                    |

The chunk counts are also in the pass trace (`probe_chunks_full`,
`probe_chunks_shallow`) and so in the phase breakdown of a slow or abandoned
pass.

## When a pass runs out of time

The loop drops a pass that exceeds its budget. Four things keep that from
turning into PRs that are never observed:

- **Open-walk reserve.** The walk of still-open PRs, and the per-execution
  families, stop starting new items while a quarter of the budget remains
  and a later group has still to run this pass. Whichever of them runs last
  has nothing to leave time for and is bounded by the yield margin alone,
  so a load that fits the budget walks every open PR and clears the stall.
  Unstarted candidates are not stamped. A candidate that is already in
  flight can still overrun the reserve; the next pass then starts with a
  later group so recovery still makes progress.
- **Rotation.** The PR walk, background nudges and the per-execution rechecks
  are ordered least recently attended first, and that memory is kept across
  passes. Subsequent passes prioritize candidates least recently attended,
  with known terminal candidates first.
  A candidate is marked when the pass starts on it, so a candidate whose own
  handling is what exhausts the budget goes to the back of the next pass
  instead of cutting every pass off in the same place. Open walk, local
  upkeep and the per-execution families rotate their starting order at the
  start of that work, so a pass dropped in the open walk still rotates. A
  mid-pass re-probe covers every candidate whose group has not started and
  merges into the snapshot, so rotation never leaves a later group without
  probe results.
- **The adaptive schedule is left alone.** An abandoned pass has no
  observations. It is not treated as a pass that saw no candidates, which
  would retire every per-PR adaptive slot; hot PRs keep being reconciled
  between sweeps.
- **Nothing is skipped or marked done unobserved.** A candidate that was not
  reached is simply attended to later. Every transition still comes from a
  probe of that PR.

## Health signal

An abandoned pass, or a pass that yields at the open-walk reserve with
open PRs still unwalked, raises a `merge_detection_stalled` issue on the
engine health report (the app's health banner, `bossctl` health output).
It is a warning after one such pass and an error after two in a row. The
body names the phase the most recent pass was cut off in, how many of that
phase's items it had finished, and the phase timings. The issue clears
itself when a pass walks every open candidate.

## Finding where the time went

Every pass publishes its timing through the metrics registry
(`bossctl metrics list --prefix merge_poller`):

| Series                          | Meaning                                                                   |
| ------------------------------- | ------------------------------------------------------------------------- |
| `merge_poller.pass_duration_ms` | Wall-clock of the most recent pass (the budget, when it was abandoned)    |
| `merge_poller.phase_ms.<phase>` | Wall-clock the most recent pass spent in each phase; `0` if never reached |
| `merge_poller.items_per_pass`   | Items the most recent pass finished, summed over phases                   |
| `merge_poller.items_unfinished` | Items the most recent pass listed but did not finish                      |
| `merge_poller.passes`           | Passes ended, completed or abandoned                                      |
| `merge_poller.pass_overrun`     | Passes that took longer than the cadence                                  |
| `merge_poller.pass_timed_out`   | Passes abandoned at the budget                                            |

The same breakdown is in the engine log on any pass that was slow:

- `merge poller: detection pass exceeded its time budget and was abandoned
mid-flight` carries `cut_off_phase`, `cut_off_item`, `items_unfinished` and
  `phases`.
- `merge poller: detection pass took longer than its own cadence` carries
  `phases`.
- `merge poller: detection pass phase timings` is logged at `info` for a
  completed pass of 10 s or more, and at `debug` otherwise.

`phases` reads `phase=<ms>ms(<done>/<total>)`, for example:

```text
list_candidates=14ms probe=910ms(1/1) terminal_walk=38ms(1/1) open_walk=178000ms(12/46)[cut off at chore_x]
```

To read it:

- **`probe` is large.** The phase always shows `(1/1)`: it is one item that
  covers the batched request plus everything done inside it. The batched
  request was slow, it fell back, or it made per-PR calls. Look for
  `batched probe graphql` lines: a response that carries errors for some PRs
  re-probes only those; a response with no usable body, or an error that
  names no PR, is retried in halves (bisected) and says so at `warn`, down
  to a single PR probed on its own. Each initial probe or refresh shares a
  deadline of the smaller of 30 seconds and one quarter of the pass's
  remaining time. Every awaited batch, individual probe and combined-status
  request is cancelled at that deadline; completed observations survive and
  unfinished URLs return retryable errors.
  Before splitting, URLs are ordered least recently attended first, with
  previously observed terminal PRs first. Starting a fallback batch or
  individual probe advances attendance, including when cancelled.
  The trace reports `probe_fallback=<ms>ms(attempted=…,completed=…,deferred=…)`
  across bisection and individual calls, counting distinct URLs per probe
  invocation. Completed counts cover usable fallback observations; deferred
  counts cover fallback URLs without a completed observation. Fallback time
  covers bisection and individual fallback spans, including
  combined-status work inside those spans; nested spans are counted once.
  `probe_combined_status` separately reports elapsed time and attempted,
  completed and deferred request counts for actual empty-rollup REST calls
  (including ETag requests). Both measurements survive cancellation and
  overlap their enclosing phase; do not add them to total pass time.
- **`open_walk` is large.** The walk is local DB work, a dozen or so short
  queries per PR. Look for `work DB connection lock timing` lines whose
  `caller` is under `merge-poller/`: a large `wait_ms` means the walk is
  queueing for the shared connection behind another holder, and the holder's
  own line names it.
- **`pr_recheck` or `late_pr` is large.** These make GitHub calls per
  execution. The GitHub call log attributes them to the `merge_poller.sweep`
  caller.
