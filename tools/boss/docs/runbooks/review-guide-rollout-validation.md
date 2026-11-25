# Automatic PR review guides: rollout validation

This runbook is the operator-run checklist that gates flipping
`review_guide_source_capture` and `review_guide_generation` on by default.
It implements the design's [real-path release-gate
requirements](../designs/automatic-pr-review-guides.md#real-boss-path-and-release-gate):
three real Boss PRs through the full capture → Astra-high generation →
viewer → feedback → merge path, with both rollout flags already wired
end to end.

See [`tools/boss/docs/designs/automatic-pr-review-guides.md`](../designs/automatic-pr-review-guides.md)
for the architecture and rationale.

**This is a human-in-the-loop exercise.** It touches real GitHub PRs, a
real Astra (`codex` / `gpt-6-astra` / `high`) job, the production Boss
app's markdown viewer, and a reviewer-authorized merge — none of that
can be done from an unattended worker session.

## Prerequisites

1. **Engine and CLI are built from this PR's head.** `GetReviewGuideRun`,
   the `ReviewGuideRun` event, and `boss review-guide show` exist only on
   this branch. The rollout flags and the Generate button already exist
   on `main`, so a production binary will pass a flag-name check and then
   fail every later inspection step after real PRs and Astra jobs have
   already been spent.

   Build and run from this checkout:

   ```sh
   bazel build --remote_download_outputs=toplevel \
     //tools/boss/engine/core:engine \
     //tools/boss/cli:boss
   ```

   Point the running Boss at that engine binary (rebuild/relaunch the
   macOS app from the same checkout if the app bundles its own engine).
   Confirm with `boss engine status`. Expected: `Boss engine is running.`
   and a socket path under the production state directory, not
   `/tmp/boss-test-…` or `/tmp/boss-shot-…`.

   Concrete CLI check (do this before any sample PR):

   ```sh
   boss review-guide show --help
   ```

   Expected: the `show` subcommand is listed and the usage line takes an
   execution id (or root task id). Then:

   ```sh
   boss review-guide show exec_not_a_review_guide
   ```

   Expected: an `unknown execution` or `is not a review-guide run` error.
   An `unknown request` / decode error means the running engine predates
   `GetReviewGuideRun` — stop, rebuild from this PR, and relaunch.

2. In the macOS app, open **Boss → Settings… (⌘,) → Feature Flags** tab.
   The two rollout flags are listed under the **review** category. Turn
   on `review_guide_source_capture` first. Expected: the flag shows
   enabled. Then turn on `review_guide_generation`. Expected: both flags
   show enabled. Generation only takes effect once source capture is also
   on — if you enable generation first, it stays inert until capture is
   on.

3. Confirm the running engine includes the guide implementation: both
   flag names appear under **Boss → Settings… (⌘,) → Feature Flags**
   (review category), and any root card with a PR URL offers **Generate
   Review Guide** (or **Regenerate Review Guide** when a readable version
   already exists). If either flag is missing from the pane, stop — the
   running engine predates the guide code; rebuild and relaunch from
   this PR's head as in Prerequisite 1.

## Finding a review-guide execution id

`boss review-guide show` is keyed by the `pr_review_guide` execution id.
Guide executions bind `work_item_id` to a comparison id, so
`boss task executions <root>` does not list them.

While a job is live:

1. `bossctl agents list` — find the row with `kind=pr_review_guide`.
2. `bossctl agents status <slot-or-name>` — copy the `exec_…` id.

After the job has finished (or if you missed the live window), inspect
by the PR's root task id instead:

```sh
boss review-guide show <root-task-id> --json
```

That resolves the latest bound guide execution on that root. Record the
returned `execution_id` in the results table so later steps can address
that specific run. `GetReviewGuideSummary` also carries optional
`execution_id` on the series and on each `ReviewGuideAttempt`.

## Step 1 — ordinary implementation PR

1. Dispatch (or pick) a small ordinary implementation change that Boss
   will open as a PR — not docs-only, not a flag-only/build-only change.
2. Wait until the PR exists and the review-guide worker has run. While it
   is generating, copy the `exec_…` id from the review-guide pane or from
   `bossctl agents list` → `bossctl agents status <slot-or-name>`
   (`kind=pr_review_guide`). If the live window has closed, use
   `boss review-guide show <root-task-id> --json` and record
   `execution_id`.
3. Confirm the root card shows the guide moving `queued` → `generating`
   → `ready` (or `failed`). Independent of CI, approval, and merge
   readiness.
4. Once `ready`, inspect the run:

   ```sh
   boss review-guide show <execution-id> --json
   ```

   Expected: JSON with `source` (canonical PR URL, observed base SHA,
   merge-base SHA, head SHA, packet hash), `prompt_version`, `model`
   (`driver` / `model` / `effort`), and `timings`
   (`source_captured_at`, `queued_at`, `started_at`, `finished_at`,
   `published_at`). Record those fields in the results table. See
   [What the timestamps measure](#what-the-timestamps-measure) before
   treating intervals as clean phases.

If any step doesn't happen within its expected window, stop and capture
engine logs before retrying — don't re-click Generate on a stuck attempt
(duplicate clicks on a live attempt are a documented no-op).

## Step 2 — docs/build-only PR

1. Open a PR whose change is documentation or build-only (no runtime
   behaviour). Boss should still capture a comparison and generate a
   guide; automatic capture skips design/investigation _doc PRs_, but a
   docs-only change on an ordinary implementation task is in scope.
2. Confirm the card reaches `ready` the same way as Step 1.
3. Open the guide (Step 4) and confirm it describes the docs/build change
   rather than inventing a runtime fix. Record the same inspection JSON
   as Step 1.

If there is no capture, or the guide invents runtime behaviour for a
docs-only change, stop and capture engine logs and the inspection JSON
before regenerating.

## Step 3 — PR that receives submitted guide feedback

1. Pick or open a third real PR (can be a throwaway implementation
   change). Wait until its guide is `ready`.
2. Continue with Steps 4–6 on **this** PR — the submitted-feedback sample
   is this one.

If this PR never reaches `ready`, or a later step routes feedback onto
a different PR than this one, stop and capture engine logs and the
inspection JSON before regenerating.

## Step 4 — open the guide in the async markdown viewer

1. From the root card, open the readable guide. Expected: the existing
   async markdown viewer, not a new surface; title/chrome identify it as
   a review guide for this PR, with version and currentness metadata.
2. Confirm the five main sections are present and that source links are
   clickable. A render capture checks appearance only — it is not
   evidence for comment dispatch or merge.
3. Record click-to-render from the existing viewer timing
   instrumentation: `MarkdownDocumentChrome` emits `phase=interactive`
   (and `phase=parse`) os_log lines once the opened document has a
   measured height. Copy `duration_ms` for `phase=interactive` into the
   results table's click-to-render column. This is the design's render
   measurement; it is part of this gate, not an optional extra.

If the viewer fails to open, shows another document, or loses identity
on refresh, stop and capture engine logs plus a screenshot of the
viewer chrome before retrying.

## Step 5 — submit a comment; it revises the PR implementation

1. In the viewer sidebar, write a comment on the guide and **submit**
   it (save-only is not this step).
2. Expected: Boss creates a **revision of the same PR's implementation**,
   not a document-edit of the guide prose and not a new chore. The
   revision lands in Doing, bound to this PR.
3. Confirm the comment stays on the guide version you wrote it against
   (original quote and version remain after later regeneration).

If submit creates a document revision, a chore, or a new PR, stop and
capture engine logs before touching the comment again.

## Step 6 — regenerate; older version and comments survive

This step has two parts. Do **6a** (required) on the feedback PR from
Step 5. Do **6b** only if you also want to check same-comparison manual
regeneration.

### 6a — new comparison from a pushed head (required)

1. Push the Step 5 feedback revision (or another small follow-up commit)
   to the **same** PR. Record the new GitHub head SHA before generating.
   Do not substitute a Regenerate click for this push: without a new
   head, capture correctly reuses the current comparison.
2. Wait for automatic recapture, or click **Regenerate Review Guide**
   _after_ the push has landed. Wait for the new attempt to reach
   `ready`. Inspect with `boss review-guide show <new-execution-id>
--json` (or the root task id). Expected: a new `attempt_id` /
   `version_id` / `request_epoch`; `source.head_sha` matches the pushed
   head, and it differs from Step 3/5's recorded head.

### 6b — same-comparison manual regeneration (optional)

1. With **no** further push, click **Regenerate Review Guide**.
2. Expected: a new `attempt_id` / `version_id` / `request_epoch`, and
   **unchanged** source SHAs (same comparison reused). Record this as a
   note, not as the Step 6a head-change evidence.

Then, for either path:

3. In the viewer, open the **older** version. Expected: its Markdown is
   unchanged, and the comment from Step 5 is still on that version with
   the original quote.
4. Confirm the new version is the one the card now offers as readable.

If the old version disappears, its comment is rewritten onto the new
prose, or the new run overwrites the old `version_id`, stop and capture
engine logs.

## Step 7 — reviewer-authorized Merge When Ready from the viewer

1. On a PR **you have explicitly chosen to merge** (the feedback PR
   after its revision is green, or another sample you are willing to
   land), open the guide and click **Merge When Ready** in the viewer.
2. Expected: the same confirmation and merge path as the Review card
   (Direct or Trunk, depending on the product). The click is yours; the
   generation job must not auto-approve.
3. Confirm the task reaches merged/done the same way a card merge would.
   Guide failure or staleness must not change merge eligibility.

If the viewer merge is missing, drives a different RPC than the card, or
merges without confirmation, stop and capture engine logs. Do not retry
merge on a PR that already has an active intent.

## Step 8 — rollback: turn generation off; versions and comments remain

1. In **Boss → Settings… (⌘,) → Feature Flags** (review category), turn
   `review_guide_generation` **off**. Leave `review_guide_source_capture`
   as it is for this step.
2. Expected: no new guide jobs start. Existing `ready` guides stay
   readable in the viewer; comments from Step 5 remain; Merge When Ready
   on the card and in the viewer still works.
3. Confirm with `boss review-guide show <execution-id> --json` on an
   earlier run (the id recorded in Step 1 or 6, or the root task id):
   the same `version_id`, `published_at`, and source identities come
   back.
4. Optional: turn `review_guide_source_capture` off as well and confirm
   already-captured packets and published versions are still retained.

If turning generation off hides existing guides, drops comments, or
disables merge, stop and capture engine logs; restore the flag to on
before leaving the machine.

## Recording the result

This checklist is the gate for the default-on flip in
[the design's real-path section](../designs/automatic-pr-review-guides.md#real-boss-path-and-release-gate).
Do not merge that flip until every step above is confirmed green. Fill
the table below **in this file** on the flip PR before merge — headings
only until the operator run; never invent timings or identifiers.

| Sample                   | PR URL | Base SHA | Merge-base SHA | Head SHA | Series id | Comparison id | Attempt id | Version id | Execution id | Request epoch | Source captured at | Queued at | Started at | Finished at | Published at | Click-to-render ms | Findings |
| ------------------------ | ------ | -------- | -------------- | -------- | --------- | ------------- | ---------- | ---------- | ------------ | ------------- | ------------------ | --------- | ---------- | ----------- | ------------ | ------------------ | -------- |
| Ordinary implementation  |        |          |                |          |           |               |            |            |              |               |                    |           |            |             |              |                    |          |
| Docs/build-only          |        |          |                |          |           |               |            |            |              |               |                    |           |            |             |              |                    |          |
| Submitted guide feedback |        |          |                |          |           |               |            |            |              |               |                    |           |            |             |              |                    |          |

Phase timestamps come from `boss review-guide show <execution-id> --json`
(`timings.*` and `source.*`). Identifiers come from the same payload.
Click-to-render is Step 4's `phase=interactive` `duration_ms`. Human
findings are free-text: material grounding/navigation defects,
comment-routing mistakes, merge-path surprises, rollback failures.

Copy source SHAs from the JSON, not from a live `gh pr view` of a merged
PR's current base/head.

### What the timestamps measure

These columns are the durable attempt/version stamps, not independent
phase clocks. Do not treat a zero or missing interval as a clean
sub-millisecond phase.

- `source_captured_at` — `pr_review_guide_source_comparisons.captured_at`.
  Packet capture time. There is no separate source-start timestamp; the
  source _duration_ is not available from this payload.
- `queued_at` — `pr_review_guide_attempts.created_at`. When the attempt
  row was inserted (queued).
- `started_at` — `pr_review_guide_attempts.started_at`, stamped by
  `bind_review_guide_execution` when the ready execution is bound,
  **before** the worker launches. `started_at` → `finished_at` therefore
  includes scheduler/launch delay as well as model time.
- `finished_at` — `pr_review_guide_attempts.finished_at`. Attempt
  terminal stamp (success, failure, cancel, or supersede).
- `published_at` — `pr_review_guide_versions.generated_at`. On a
  successful publish, `finished_at` and `generated_at` are written with
  the same `now`, so the derived publication interval is **always zero**.
  That is not evidence that publication was instantaneous; the engine
  does not persist a distinct publication boundary.
- Click-to-render — viewer `phase=interactive` os_log, measured in
  Step 4. Independent of the engine payload.

A failed run has `error` and `retries` on the inspection payload; use
those rather than inferring the failure from empty version fields.
