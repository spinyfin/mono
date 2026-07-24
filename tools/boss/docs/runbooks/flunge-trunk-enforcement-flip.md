# Flunge Trunk Queue Enforcement: Flag-Day, Verification, and Emergency Bypass

This runbook covers flipping `brianduff/flunge` from "Trunk merge queue available but optional" to "Trunk merge queue enforced" (direct pushes to `main` rejected), the verification steps that confirm the flip took, an emergency-bypass procedure for a repo admin who needs to land a change without going through the queue, and the symmetric rollback.

See [`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md) (§"Misconfiguration detection and enforcement flag-day") for the design rationale. This doc is the operator-executed sequence that design section pointed at.

---

## Background

Before enforcement, both merge paths work on flunge: a human (or a misconfigured product) can still run `gh pr merge` directly, and Boss can submit through Trunk's merge queue when `merge_mechanism = trunk_queue`. Enforcement makes the queue the _only_ way `main` moves — GitHub branch protection restricts who/what can push to `main` to the Trunk GitHub app, so a direct `gh pr merge --auto --squash` hard-fails with a push-restriction error instead of silently landing.

Boss already has a merge-mechanism setting per product (`direct` | `trunk_queue`) and, on the `direct` path, detects push-restriction-shaped `gh` failures and files an attention item naming the likely fix (`engine/core/src/merge_mechanism.rs`: `is_push_restriction_error`, `PUSH_RESTRICTION_ATTENTION_KIND`). This runbook assumes flunge's product record is already `merge_mechanism = trunk_queue` and that Boss has landed at least one merge and survived at least one eviction-remediation cycle through the queue before enforcement is turned on — do not flip enforcement on a repo that hasn't proven the queue path yet.

---

## 0. Prerequisites

Before touching branch protection:

1. Confirm the Trunk API token is configured and healthy:

   ```sh
   boss engine trunk status
   ```

   Expect `Trunk API token configured (...)` and `Queue smoke check: ok`. If the token is missing or the smoke check fails, fix that first (`boss engine trunk set-token`, piping the token via stdin) — enforcement with a dead token means nothing can merge.

2. Confirm flunge is on the queue mechanism:

   ```sh
   boss product set-merge-mechanism flunge --mechanism trunk_queue
   ```

   (Idempotent — re-running with the already-set value is a no-op confirmation, not a behavior change.)

3. Confirm at least one PR has merged through the queue via Boss's merge button (`trunk_enqueued` action, card visible in the Merging lane, terminal `merged` observed on the GitHub side) **and** at least one eviction has gone through the auto-resubmit path. If neither has happened yet, do a deliberate low-risk merge and, if practical, a deliberate eviction (e.g. push a failing commit to an already-queued PR) before proceeding — enforcement is not the place to discover the queue path is broken.

---

## 1. The flip sequence

All steps are in flunge's GitHub branch-protection settings: `https://github.com/brianduff/flunge/settings/branches` (or `settings/rules` for the newer rulesets UI — check which one flunge actually uses before editing, they display differently).

1. **Restrict pushes to `main` to the Trunk GitHub app.** Under the `main` protection rule, enable "Restrict who can push to matching branches" (or the ruleset-equivalent "Restrict updates") and add the Trunk GitHub App as the sole allowed actor. Remove any individual users/teams that were previously allowed to push directly — that's the actual enforcement step; everything else in this section is cleanup so the queue can function once pushes are restricted.
2. **Disable "Require branches to be up to date before merging."** The queue already re-tests each PR against current `main` before merging (that's what `trunk-merge/*` construction branches are for) — leaving this on double-serializes merges and can wedge the queue waiting for a freshness check that's redundant with the queue's own mechanism.
3. **Exclude branches under the `trunk-temp/` and `trunk-merge/` prefixes from the branch-protection rule** (or from the ruleset's target-branch patterns, whichever flunge uses). Trunk creates and force-pushes these working branches as part of running the queue; if branch protection applies to them too, Trunk's own operations get blocked by the rule meant to protect `main`.

---

## 2. Verification

Do these in order, immediately after step 1:

1. **Manual direct merge must hard-fail.** Open (or reuse) a throwaway PR against flunge and run:

   ```sh
   gh pr merge --auto --squash <pr-url>
   ```

   Expect a push-restriction / rule-violation error (GH013, "repository rule violations found", "changes must be made through a pull request", or similar — see `PUSH_RESTRICTION_MARKERS` in `engine/core/src/merge_mechanism.rs` for the exact substrings Boss itself watches for). If this succeeds, enforcement did not take — go back to §1 step 1 and re-check the restriction is actually saved and scoped to `main`.

2. **Boss merge must succeed through the queue.** Click merge on a real (or another throwaway) flunge PR in the Boss app, or run the equivalent CLI/RPC path. Confirm:
   - The card moves into the Merging lane with a Trunk queue badge (position, then "Testing", then merged).
   - `boss engine trunk status` and/or `getQueue` behavior looks sane (no `PAUSED`/`DRAINING` attention items firing).
   - The PR eventually shows `merged` on GitHub and the Boss task transitions to done via the existing GitHub-side terminal-detection path (Trunk's own `merged` state is not what triggers this — see the design doc's state-machine section).

3. **No attention items about push restrictions should be firing for flunge going forward.** If `merge_mechanism` drifted back to `direct` for any reason post-flip, the next direct-merge attempt will file a `direct_merge_push_restriction` attention item automatically (title: "Direct merge blocked by a push restriction — product may need merge_mechanism=trunk_queue") — treat that as a live signal, not something you need to poll for.

---

## 3. Emergency-bypass runbook

Use this when a repo admin needs to land a change on flunge's `main` immediately and cannot wait for the queue (incident response, a change the queue itself can't currently process, etc.).

1. **Pause the Trunk queue first**, from the Trunk web app: set the flunge/`main` queue to `PAUSED`. This is the step that prevents the bypass from racing the queue — without it, Trunk may be mid-merge on another PR's `trunk-merge/*` branch while an admin is also pushing directly, and the two can conflict or produce an out-of-order `main`.
2. **Temporarily lift the push restriction** in GitHub branch protection (`https://github.com/brianduff/flunge/settings/branches`): either disable "Restrict who can push to matching branches" entirely, or add the admin doing the bypass to the allowlist alongside the Trunk GitHub app. Prefer the narrower allowlist addition when the bypass is by a single named admin — it's the smaller, more auditable change and is faster to revert precisely.
3. **Merge the emergency change** via the normal `gh pr merge` (or a direct push, if that's what the incident requires). This is intentionally the one window where a direct merge on flunge is expected to succeed.
4. **Restore the restriction** immediately after: remove the temporary allowlist entry (or re-enable the restriction if it was disabled outright), so flunge is back to Trunk-app-only push access.
5. **Unpause the Trunk queue** in the web app (`RUNNING`).
6. **If Boss was tracking the bypassed PR** (it had an active `trunk_merge_intents` row — i.e. someone had clicked merge in Boss before the bypass happened), do nothing extra: reconciliation is automatic. Terminal detection lives on the GitHub side (the existing merge poller observes the PR's `merged` state directly), not on Trunk's own `merged` state, so a PR that lands via direct push during the bypass window is still picked up and marked done the same way a queue-merged PR is. If it doesn't reconcile within a normal poll cycle (Hot tier is 15 s), check `boss engine trunk status` and the task's attention items before assuming something is stuck.

---

## 4. Rollback

Rollback is symmetric and safe at any time — nothing in the engine assumes enforcement is on:

1. Remove the push restriction on `main` in GitHub branch protection (same screen as §1).
2. Set flunge back to the direct mechanism:

   ```sh
   boss product set-merge-mechanism flunge --mechanism direct
   ```

3. Re-enable "Require branches to be up to date before merging" if it's wanted back (optional — it was only disabled to avoid double-serializing with the queue, and that reason goes away once the queue isn't gating merges).
4. Leave the `trunk-temp/` and `trunk-merge/` branch-protection exclusions in place — they're harmless when the queue isn't enforced and saves re-adding them if enforcement is turned back on later.

There is no data migration or engine-state cleanup needed: `merge_mechanism = direct` is the same code path flunge used before this project existed, and any still-active `trunk_merge_intents` rows simply stop being submitted-to going forward (existing rows are historical, not something rollback needs to touch).

---

## 5. Troubleshooting

| Symptom                                                        | Likely cause                                                                                                                                | Action                                                                                                                                  |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `gh pr merge` on flunge still succeeds after §1                | Restriction not saved, scoped to the wrong branch, or applied only under branch protection while flunge actually uses the newer rulesets UI | Re-check `settings/branches` **and** `settings/rules`; confirm the rule targets `main` specifically                                     |
| Boss merge click never leaves "queued"                         | Trunk queue is `PAUSED`/`DRAINING`, or the token is dead                                                                                    | `boss engine trunk status`; check for a queue-paused attention item / banner in the Merging lane header                                 |
| `direct_merge_push_restriction` attention item fires post-flip | A product (possibly flunge itself, if `merge_mechanism` drifted) is still on `direct` while its repo is queue-enforced                      | `boss product set-merge-mechanism <product> --mechanism trunk_queue`                                                                    |
| Emergency-bypass PR never shows as merged in Boss              | Reconciliation hasn't run yet, or the task had no active merge intent to begin with (nobody had clicked merge in Boss)                      | Wait one Hot-tier poll cycle (~15 s); if still stuck, check `boss engine trunk status` and the task's attention items before escalating |
| Trunk queue stuck `PAUSED` after an emergency bypass           | Step 5 of §3 (unpause) was missed                                                                                                           | Unpause from the Trunk web app; queued PRs resume from where they left off                                                              |

---

## 6. Related docs

- [Trunk merge queue integration design](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md) — full architecture, state machine, and the misconfiguration-detection code this runbook exercises.
- [Flunge Buildkite pipeline reference](../designs/flunge-buildkite-pipeline-reference.md) — flunge's CI shape, relevant if an emergency bypass also needs to reason about which CI checks a direct push will run.
