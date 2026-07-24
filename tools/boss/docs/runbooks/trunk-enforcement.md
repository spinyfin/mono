# Trunk enforcement flag-day and emergency bypass

This runbook covers flipping `brianduff/flunge` to Trunk queue-only enforcement (main pushes restricted to the Trunk GitHub app), and the emergency-bypass procedure for landing a change when the queue can't be used.

See [`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md) for the architecture and design rationale.

## Enforcement flip sequence

Operator-executed, after the engine path has merged N ≥ 1 successful queue-backed merges.

1. Confirm `boss engine trunk status` is green and flunge's `merge_mechanism = trunk_queue` has been live through at least one full merge and one eviction-remediation cycle.
2. In GitHub branch protection for `main` on `brianduff/flunge` (per Trunk's enforcement docs):
   - Restrict pushes to `main` to the Trunk GitHub app.
   - **Disable** "require branches to be up to date before merging" — the queue owns freshness; leaving it on double-serializes.
   - Exclude `trunk-temp/**` and `trunk-merge/**` from branch-protection rules so Trunk can manage its working branches.
3. Verify: a manual `gh pr merge` on a throwaway PR now fails with a push-restriction error; a Boss merge-button merge succeeds through the queue.
4. See "Emergency bypass" below for the documented procedure if you need to land a change outside the queue.
5. Rollback is symmetric: remove the push restriction and set `merge_mechanism` back to `direct`; nothing in the engine assumes enforcement.

## Misconfiguration detection

If a product is still `direct` and enforcement is enabled, `gh pr merge` fails — the handler surfaces `gh` stderr as `WorkError`, and a pattern match on push-restriction/rule-violation errors upgrades it to an attention item that names the fix ("flunge appears to be queue-enforced; set merge_mechanism=trunk_queue"). Conversely, `trunk_queue` without a working token is covered by the auth flow (`boss engine trunk set-token` / `trunk status`). Both states fail loudly on first use; neither can silently merge around the queue.

## Emergency bypass

Use this when a change must land on `main` and the Trunk queue can't be used (queue outage, urgent hotfix, etc.):

1. **Pause the Trunk queue first** (`PAUSED` via the Trunk web app) — this avoids racing the queue during the bypass.
2. Repo admin temporarily lifts the push restriction (or adds themselves to the allowlist) in GitHub branch-protection settings for `main` on `brianduff/flunge`.
3. Merge the emergency change directly.
4. Restore the push restriction.
5. If Boss was tracking that PR, no manual reconciliation is needed — the normal merged-observation path reconciles it, since terminal merge detection is GitHub-side, not Trunk-side.
