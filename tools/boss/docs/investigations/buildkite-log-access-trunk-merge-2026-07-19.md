# Buildkite log access for `trunk-merge/*` failures + Trunk eviction dedup key

- **Date:** 2026-07-19
- **Execution:** `exec_18c3e754c45f7620_e73` (investigation_implementation)
- **Work item:** Investigate Buildkite log access for trunk-merge/\* failures
- **Parent project:** Trunk merge queue integration ([design doc](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md))
- **Feeds:** task 6 (eviction → ci_watch integration), task 3 (Trunk token provisioning)
- **Method:** live, read-only probes against Buildkite org `flunge` (via `bk`), GitHub `brianduff/flunge` / `spinyfin/mono` (via `gh`), and docs.trunk.io — no code changed, no queue entries touched

This investigation answers two pre-implementation questions from the design's risk list: can the engine fetch failure logs for a Trunk `trunk-merge/*` construction-branch CI run, and is the eviction dedup key (`getSubmittedPullRequest.id` + `stateChangedAt`) stable per queue episode. The queue has been live on flunge since 2026-07-20 (UTC) and had processed 15 episodes at probe time, giving real construction-branch builds to test against — including failed builds on Trunk-created branches.

## TL;DR

**Log access: yes, verified end-to-end — but not by the route the design assumed.** `prSha` does _not_ locate the failing build (it is the PR's head commit, not the tested construction commit), and GitHub's check-runs API returns _zero_ results for construction commits on flunge (Buildkite posts one legacy build-level commit status there, with no job-uuid fragment). The reliable, verified path is Buildkite-side and branch-based: episode branch name → org-wide build lookup → failed jobs from the build JSON → `bk job log <job-uuid> -p <pipeline> -b <build>`. One real defect found on the way: `BuildkiteLogReader` invokes the bare `bk job log <uuid>` form, which fails with `failed to resolve a pipeline` unless the process cwd is a repo checkout with a resolvable pipeline — the reader needs the pipeline slug and build number threaded through.

**Dedup key: direct verification is blocked on the Trunk API token (task 3), which does not exist yet on the operator machine or in Boss code.** `getSubmittedPullRequest` cannot be called by anything on this machine today, so a deliberate eviction now could not observe `id`/`stateChangedAt` and was not performed (the queue was actively merging production PRs at probe time). Indirect evidence is strong that a per-episode UUID exists and is distinct from the queue id. Either way, the composite key `(id, stateChangedAt-of-the-failed-transition)` is unique per episode under _both_ candidate semantics of `id`, so task 6 does not need to block on the answer. A 10-minute post-token verification runbook is included below.

## Anatomy of a queue episode (observed live)

Episode for flunge PR #1007 (merged through the queue 2026-07-20T04:46Z), reconstructed from Buildkite and GitHub records:

| Artifact                                                                | Value                                                                                                                                       |
| ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| PR head commit (`prSha`)                                                | `6bc62096cc` (branch `boss/exec_18c3e59158cd0068_e02`)                                                                                      |
| Episode UUID (in branch names)                                          | `d772adf2-6ba4-4abb-a3c2-2b785fe08af3`                                                                                                      |
| Construction branches                                                   | `trunk-temp/pr-1007/<uuid>` and `trunk-merge/pr-1007/<uuid>`                                                                                |
| Construction commit                                                     | `01775f1e7a` — a merge commit by `trunk-io[bot]`, parents = [`9eb365a043` (queue base), `6bc62096cc` (PR head)]                             |
| Construction commit message                                             | `Merging 6bc62096cc… into trunk-temp/pr-1007/d772adf2-…`                                                                                    |
| Buildkite builds on that commit                                         | `flunge-ci` #2365 (branch `trunk-temp/…`) and #2364 (branch `trunk-merge/…`); plus `flunge-release-frontend` #1662 on the same branch       |
| Trunk's own check run (on the **PR head**, not the construction commit) | `Trunk Merge Queue (main)`, app `trunk-io`, details_url `https://app.trunk.io/flunge/merge-queue/c1478ade-ef63-4ba9-86de-b45801e5fb5e/1007` |

Points that matter for the implementation:

- **Trunk builds each episode on two branches.** `trunk-temp/pr-<N>/<uuid>` (candidate assembly; often several builds per episode) and `trunk-merge/pr-<N>/<uuid>` (the gating test branch). Both carry the same episode UUID. Only `trunk-merge/*` outcomes are eviction signals: build #2282 (`trunk-temp/pr-978/…`) **failed** while #2285 (`trunk-merge/pr-978/…`) passed and PR #978 merged normally. Evidence gathering must filter on the `trunk-merge/` prefix and must not treat `trunk-temp/*` failures as evictions.
- **More than one pipeline builds the construction branch.** Both `flunge-ci` and `flunge-release-frontend` ran on the episode branch. The failing build may live in either, so discovery should query org-wide, not per-pipeline.
- **The episode UUID is not the queue id.** Trunk's check-run/comment URLs embed `c1478ade-ef63-4ba9-86de-b45801e5fb5e` for PRs #978, #1006, and #1007 alike — that UUID is the _queue_. The per-episode UUID appears only in the construction branch names.
- **GitHub construction refs are ephemeral; Buildkite records are durable.** Minutes after the merges, `git/matching-refs/heads/trunk-merge/` and `…/trunk-temp/` on `brianduff/flunge` were both empty — every construction ref is deleted at episode end. The construction _commits_ remain fetchable by SHA on GitHub (commit object, statuses) after ref deletion, but the durable, enumerable record of the episode is Buildkite's build list.

## Finding 1 — `prSha` does not locate the failing build

`getSubmittedPullRequest` returns `prNumber, prTitle, prSha, prBaseBranch, prAuthor` — PR attributes. The observed construction commit confirms `prSha` (the PR head) is the _second parent_ of the tested commit, not the tested commit itself. Verified concretely: `GET /pipelines/flunge-ci/builds?commit=6bc62096cc…` (PR head) returns only build #2358 on the PR's own branch — it does **not** find construction build #2364. Querying by `?commit=01775f1e7a…` (construction commit) returns both #2364 and #2365 — but nothing in the documented Trunk API response carries that SHA or the construction branch name.

So the design-doc phrasing "which the Trunk PR object supplies via `prSha`/queue branch head" holds only for its second half, and only indirectly: the queue branch head is discoverable, but from the **branch name**, not from any SHA Trunk hands us.

## Finding 2 — the GitHub-side evidence path (`fetch_failing_checks_for_commit`) returns nothing on flunge

The GH-native rebounce path fetches failing checks via REST `repos/{owner}/{repo}/commits/{sha}/check-runs` (`boss_github::fetch_failing_checks_for_commit`, `tools/boss/github/src/check_runs.rs:97`). Verified against construction commit `01775f1e7a`:

- **Check-runs: `total_count: 0`.** Flunge's Buildkite integration publishes legacy commit _statuses_, not check runs. The fetcher returns `[]` (best-effort, no error), so a naive reuse of the rebounce evidence path would silently produce no target URL, no job id, and no log excerpt.
- **Combined status: present but weak.** Two contexts (`buildkite/flunge-ci`, `buildkite/flunge-release-frontend`), each with a build-level `target_url` (`https://buildkite.com/flunge/flunge-ci/builds/2365`) and **no `#<job-uuid>` fragment** — `parse_provider_job_id` would return `None`. Worse, the context pointed at build **#2365, the `trunk-temp` sibling**, not the `trunk-merge` build #2364: both builds share the commit SHA and the status context, so last-writer-wins decides which build the status links to. The commit status is not a trustworthy pointer to the gating build.
- **Contrast with mono** (why the current code works there): mono's pipeline posts _per-job_ statuses (`buildkite/mono/checks`, `buildkite/mono/bazel-build-test`, …) whose target URLs _do_ carry `#<job-uuid>` fragments (verified on `spinyfin/mono` head). The job-id extraction path is sound where the pipeline is configured for per-step GitHub statuses; flunge-ci is not.

## Finding 3 — the Buildkite-side path works, verified end-to-end

All of the following were exercised live with the already-authenticated `bk` CLI (org `flunge`):

- **Org-wide build lookup by exact branch** — the best single call, covering all pipelines at once: `bk api "/builds?branch=trunk-merge/pr-1007/d772adf2-…"` returned `flunge-ci` #2364 and `flunge-release-frontend` #1662.
- **Per-pipeline filters** also work: `?commit=<construction-sha>` (returns temp + merge builds; filter branch prefix), `?branch=<exact>`, and `?state=failed` (server-side; combine with client-side `trunk-merge/pr-<N>/` prefix matching when the episode UUID is unknown). Branch filtering is exact-match only — no server-side wildcard.
- **Failed jobs are enumerable from the build JSON** (`.jobs[] | select(.state=="failed") | .id`), including on a real failed Trunk-created-branch build (#2337, `trunk-temp/pr-999/…`, one failed job, exit 128).
- **Job log fetch works with full coordinates**, on both a passed `trunk-merge` job and the failed #2337 job: `bk job log <job-uuid> --pipeline flunge-ci --build-number <N>` returned complete logs (the #2337 log shows the actual failure: `fatal: ambiguous argument '0425788633…^'` during pipeline upload — a force-push race on the temp branch, incidentally exactly the infra-flavored failure class `classify_pre_triage` retriggers).

**Defect found:** `BuildkiteLogReader::read_log_full` runs the bare form `bk job log <job-id>` (`tools/boss/engine/ci-log-reader/src/lib.rs:104`). Verified: outside a repo checkout with a resolvable pipeline, that form fails with `Error: failed to resolve a pipeline` — `bk` requires `--pipeline` and `--build-number` (its own help says so). The engine does not run with its cwd inside a flunge checkout, so the engine-side excerpt fetch can never work for flunge via this reader as-is. The prompt-side helper already does this correctly (`render_bk_log_commands`, `engine/core/src/runner.rs:3272`, emits `bk job log --pipeline <slug> --build-number <N> <uuid>`), and both coordinates are parseable from any Buildkite build URL with the existing `parse_buildkite_pipeline_slug` / `parse_buildkite_build_id` helpers. The reader's Buildkite arm needs the coordinates threaded through (trait change or a Buildkite-specific constructor carrying pipeline+build).

### Recommended evidence recipe for task 6 (eviction → ci_watch)

On observing Trunk state `failed` for PR `<N>`:

1. `GET /v2/organizations/flunge/builds?branch=trunk-merge/pr-<N>/<episode-uuid>` if the episode UUID is known (see dedup-key section — `id` is likely exactly this UUID; confirming that is part of the post-token runbook). Otherwise list recent failed builds per pipeline (`?state=failed&per_page=100`) and prefix-match `trunk-merge/pr-<N>/`, taking the newest at/before `stateChangedAt`.
2. From each failed build's JSON: pipeline slug, build number, failed job uuids, `web_url` (this substitutes for the missing GitHub `target_url`; it has the canonical `https://buildkite.com/<org>/<pipeline>/builds/<n>` shape the existing parsers accept).
3. Fetch excerpts with full coordinates (`bk job log <uuid> -p <pipeline> -b <n>`), after the reader fix above.
4. Do not consult `repos/…/commits/<sha>/check-runs` or the commit status for the construction commit on flunge — the former is empty, the latter may point at the `trunk-temp` sibling build (Finding 2).

One coordination note: eviction also flips Trunk's own `Trunk Merge Queue (main)` check run on the **PR head** to failure. Pre-enforcement it is not a required check, but post-enforcement it will be — at which point a queue eviction also looks like a failing required check on the PR head to `statusCheckRollup` consumers. This is concrete support for the design's suppression rule (eviction remediation open ⇒ suppress `on_ci_failure_detected` for the same head SHA); task 6 should make sure the suppression covers a failure whose only failing check is the `trunk-io` app's check run.

## Finding 4 — eviction dedup key: blocked on token; safe composite key available now

What was established:

- **Documentation is silent.** Trunk's endpoint reference documents `getSubmittedPullRequest.id` as a bare `type: string` with no description; `stateChangedAt` and `prSha` are equally undescribed; the webhook docs describe payload fields only loosely and defer schemas to the Svix portal. No documented answer exists for id-per-episode vs id-per-PR.
- **No historical evidence exists.** Across the queue's entire history to date (800 builds ≈ June 25 → now; trunk builds began 2026-07-20 UTC), all 15 `trunk-merge/*` builds passed — **no eviction has ever occurred** — and no PR appears with two episode UUIDs, so resubmission behavior has never been exercised.
- **No token, no plumbing.** `BOSS_TRUNK_API_TOKEN` is unset, no keychain item exists under `dev.spinyfin.boss.trunk` (or any trunk-named service), and the repo contains no Trunk token plumbing yet (task 3 is unstarted). Nothing on this machine can call `getSubmittedPullRequest` today. A deliberate eviction _now_ therefore could not observe `id`/`stateChangedAt` at all — and the queue was actively merging production Boss PRs at probe time (5 merges in the hour before this probe), so a blind scratch eviction would have delayed real merges for zero information. The deliberate-eviction experiment must ride token provisioning; runbook below.
- **Indirect evidence favors per-episode ids.** Trunk mints a fresh UUID per submission episode and stamps it into both construction branch names; that UUID is distinct from the queue id. It would be surprising (though not impossible) for `getSubmittedPullRequest.id` to be neither of these.

**Recommendation for task 6, valid under either semantics:** key `ci_remediations` idempotency on `(trunk_entry_id, stateChangedAt)` captured at the moment the poller first observes `state == failed`. If `id` is per-episode, the pair is trivially unique per episode. If `id` is per-PR-stable, the pair is still unique per episode because each episode's `failed` transition carries a distinct `stateChangedAt`. Repeat sweeps observing the same stuck `failed` state see the same pair (idempotent), and the design already fires only on `failed` (never on `pending_failure`), so the timestamp is stable for the lifetime of the episode's terminal state. Record `(prNumber, prSha, stateChangedAt)` alongside for provenance; it is the documented fallback key and costs nothing to store. The only loss-mode either key shares is a _missed_ observation (human resubmits before the next sweep) — that is a poller-cadence question, covered by the design's `listPullRequests since=` reconciliation backstop, not a key-shape question.

### Post-token verification runbook (~10 minutes, do first thing after task 3 lands)

1. Create a scratch branch on `brianduff/flunge` with a change that deterministically fails `flunge-ci` (e.g. break a backend test); open a scratch PR.
2. `POST /v1/getSubmittedPullRequest` before submission (expect not-found), then submit via `POST /v1/submitPullRequest`; immediately record `{id, stateChangedAt, prSha}` — **check whether `id` equals the `trunk-merge/pr-<N>/<uuid>` branch UUID** that appears in Buildkite within seconds (this also decides whether step 1 of the evidence recipe can use the direct branch-name form).
3. Poll until `state == failed` (flunge CI ≈ 1 minute); record `{id, stateChangedAt}` at `failed`. Confirm a failed `trunk-merge/pr-<N>/<uuid>` build exists and its failed-job log is fetchable per the recipe above.
4. Resubmit the same PR; record `{id, stateChangedAt}` for the second episode. `id` changed ⇒ per-episode (primary key confirmed); `id` unchanged ⇒ per-PR (composite key still holds; note it in the task-6 PR).
5. `POST /v1/cancelPullRequest`, close the scratch PR, delete the branch.

## Follow-up code changes (for the human to file — none made here)

1. **`BuildkiteLogReader` needs pipeline+build coordinates** (Finding 3). Bare `bk job log <uuid>` cannot work from the engine process. Affects the existing mono excerpt path too whenever the engine's cwd is not a repo checkout, and is a prerequisite for task 6's excerpt fetch. Filed as a followup proposal with this run.
2. **(Optional, flunge repo)** configure per-step GitHub commit statuses on `flunge-ci` (mono-style `notify: github_commit_status`) if a GitHub-side evidence path for flunge is ever wanted; not required if task 6 adopts the Buildkite-side recipe.

## Open Questions

- `getSubmittedPullRequest.id` semantics (per-episode vs per-PR) and whether `id` equals the construction-branch UUID — resolvable only via the post-token runbook above; the recommended composite key makes task 6 safe to build before the answer arrives.
- Whether `prSha` is snapshotted at submission or re-read from the live PR head during an episode — same runbook can answer it (push to the scratch PR mid-episode) if it ever matters; nothing in the current design depends on it.
