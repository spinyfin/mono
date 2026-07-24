# Trunk merge queue: pre-enforcement validation

This runbook covers the operator-run exercise required before flipping
flunge to queue-only branch-protection enforcement: one real merge and one
deliberate eviction through the full Trunk-queue-backed path, with Boss
already wired end to end (design items 1–9 are merged). It intentionally
stops short of the enforcement flip itself — see
[the design's flag-day section](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md#misconfiguration-detection-and-enforcement-flag-day)
for that, once this checklist is green.

See [`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md)
for the architecture and rationale.

**This is a human-in-the-loop exercise.** It touches the real
`brianduff/flunge` repo and the real Trunk queue with production
credentials, and requires watching the Boss app's Merging lane update live
— none of that can be done from an unattended worker session.

## Prerequisites

1. `boss product set-merge-mechanism flunge --mechanism trunk_queue` has
   been run (idempotent to re-run; confirm with `boss product show flunge`
   or equivalent).
2. A Trunk org API token is configured: `boss engine trunk set-token` (reads
   from stdin/prompt), then `boss engine trunk status` should report
   `configured: true` with a **passing live queue check** against flunge.
   If `queue_check` is missing or the note says the live check isn't wired
   yet, that's a blocker — the smoke check must exist before trusting the
   rest of this checklist (see the "Confirms boss engine trunk status is
   green" requirement below).
3. The Boss macOS app is running against the real production engine (not a
   test-fixture instance) so you can watch the Merging lane.

## Step 1 — one real merge through the queue

1. Pick a small, low-risk, already-approved flunge PR (or open a trivial
   throwaway change against flunge if none is queued).
2. Click merge on the Review card in the Boss app.
3. Confirm the click response says "Submitted to Trunk merge queue" (the
   `trunk_enqueued` action), not the generic direct-merge message.
4. Watch the card move into the **Merging** section of the Done column
   within a few seconds (the `PrReconcilerKick` immediate reprobe).
5. Watch `MergeQueueBadge` progress: position → `Testing` → `Merging…` as
   Trunk moves the PR through `pending`/`testing`/`tests_passed`, refreshing
   at Hot-tier cadence (≤ ~15–30 s staleness).
6. Confirm terminal state: once Trunk merges the PR, the existing
   GitHub-side merged-observation (not Trunk's `merged` state) should detect
   it and the task should move to `done` — Merging card disappears, task
   lands in Done outside the Merging section.

If any step doesn't happen within its expected window, stop and capture
engine logs before retrying — don't re-click merge on a stuck intent (the
merge button self-hides once queued and duplicate clicks on an active
intent are a documented no-op).

## Step 2 — one deliberate eviction (scratch PR)

1. Open a scratch PR against flunge from a throwaway branch that
   deliberately fails CI (e.g. a change with a failing test or a `false`/
   nonzero-exit step added to the Buildkite pipeline for that branch only —
   do not touch the real `flunge-ci` pipeline definition).
2. Click merge on it in the Boss app so it enters the Trunk queue.
3. Wait for Trunk to run combined CI on the `trunk-merge/*` construction
   branch and evict the PR (`failed`, possibly via `pending_failure` first).
4. Confirm the card leaves Merging and snaps back to Review with
   `blocked: ci_failure` and a `CiFailureBadge` (`used/budget`).
5. Confirm an engine-triggered **revision** task appears in Doing
   (`created_via = "ci-fix:<crm_id>"`), and that `ci_remediations` recorded
   `failure_kind = 'trunk_queue_eviction'` (check via `boss` CLI/DB
   inspection tooling, not direct DB access).
6. Push the fix (revert the deliberate break) on the revision's branch and
   let head-branch CI go green.
7. Confirm **auto-resubmit**: without a fresh merge click, the intent
   resubmits (`submit_count` increments) and the card returns to Merging.
8. Confirm terminal merged-observation as in Step 1.

## Step 3 — confirm budget and coordination behavior (optional but recommended)

- If you have appetite, run the eviction twice in a row on the same PR to
  confirm the shared `ci_attempt_budget` (default 3) is actually consumed
  and that exhaustion produces `blocked: ci_failure_exhausted` plus an
  attention item, rather than an infinite resubmit loop.
- Confirm a `PAUSED`/`DRAINING` Trunk queue state (pause it briefly from the
  Trunk web app while an intent is active, if safe to do so) surfaces as a
  deduplicated attention item and a "Trunk queue paused/draining" banner in
  the Merging section header, not per-card noise.

## Recording the result

This checklist is the gate for
[design item 10](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md#proposed-implementation-task-breakdown)'s
enforcement flip. Do not proceed to branch-protection changes on
`brianduff/flunge` until every step above is confirmed green. The design's
attentions file
([`trunk-merge-queue-integration-queue-backed-merges-merging-ui.attentions.json`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.attentions.json))
also has an open question on _when_ to flip relative to this bake period —
resolve that alongside recording these results, before running the flag-day
sequence.
