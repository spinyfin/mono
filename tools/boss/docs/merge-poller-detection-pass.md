# Merge poller detection pass

The merge poller runs one full detection pass a minute. A pass lists every
work item with a PR, probes all of those PRs in one GitHub request, records
what the probe says, and then runs the upkeep that rides the same cadence.
This page covers the order a pass works in, what happens when it runs out
of time, and how to find out where its time went.

Source: `tools/boss/engine/merge-poller/src/pass.rs` (the pass),
`walk_order.rs` (the rotation), `pass_trace.rs` (timing and the health
signal), `schedule.rs` (the loop and the budget).

## Order of a pass

A pass has a hard budget of three cadences (180 s at the 60 s cadence). It
is ordered so that what must not be missed comes first and what can be slow
comes last.

| Phase                                                         | What it does                                                             | Cost scales with                    |
| ------------------------------------------------------------- | ------------------------------------------------------------------------ | ----------------------------------- |
| `list_candidates`                                             | The candidate-list queries                                               | Local DB                            |
| `merge_disarm`                                                | Turns off auto-merge for merges conflict resolution stopped              | Pending disarms, usually zero       |
| `probe`                                                       | One batched GraphQL request for every tracked PR                         | One round trip                      |
| `terminal_walk`                                               | Records every PR the probe reports merged or closed                      | PRs that just merged or closed      |
| `open_walk`                                                   | Upkeep of the PRs that are still open (CI, conflicts, merge-queue state) | Tracked open PRs, local DB          |
| `stranded_ci`, `stranded_blocked`                             | Recovers remediation that lost its worker or its signal                  | Usually zero items                  |
| `queue_rebounce`                                              | One batched merge-queue dequeue query, then per-PR checks                | One round trip                      |
| `stalled_reviewer`, `review_admission`, `review_batch_upkeep` | Reviewer fallback and review-pool upkeep                                 | Local DB                            |
| `pr_recheck`, `background_nudges`, `late_pr`                  | Per-execution PR detection for workers with no PR bound yet              | Live workers, GitHub calls per item |

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
  passes. Whatever one pass did not reach is what the next pass starts with.
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
  to a single PR probed on its own. The whole fallback shares a 30 s budget;
  PRs not reached come back as retryable errors, and the per-PR fallback
  starts at a different PR each time so the same ones are not always cut off.
  The `per-PR probe fallback finished` line gives requested / probed /
  deferred counts and elapsed time. Separately, every probed PR with an empty
  CI rollup makes one more serial `gh api` call for the commit's combined
  status (ETag-cached, so a `304` costs no quota but still spawns `gh`);
  that time is inside `probe` too and is not broken out.
- **`open_walk` is large.** The walk is local DB work, a dozen or so short
  queries per PR. Look for `work DB connection lock timing` lines whose
  `caller` is under `merge-poller/`: a large `wait_ms` means the walk is
  queueing for the shared connection behind another holder, and the holder's
  own line names it.
- **`pr_recheck` or `late_pr` is large.** These make GitHub calls per
  execution. The GitHub call log attributes them to the `merge_poller.sweep`
  caller.
