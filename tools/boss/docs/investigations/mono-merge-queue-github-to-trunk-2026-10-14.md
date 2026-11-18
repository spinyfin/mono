# Investigation: moving mono's merge queue from GitHub native to trunk.io

- **Date:** 2026-10-14
- **Kind:** investigation (read-only). No code, CI, GitHub, Buildkite or Trunk settings were changed, and `boss product set-merge-mechanism` was not run.
- **Trigger:** mono's GitHub-native merge queue stalled on 2026-10-14. Two PRs sat `QUEUED` for 20+ minutes, GitHub never created a `gh-readonly-queue/*` branch, Buildkite was healthy, and a GitHub support ticket is open.
- **Question:** can `spinyfin/mono` move to Trunk's merge queue with no change to Boss the product, and what has to change in the mono repo, in GitHub, and in Trunk?
- **Related:** [`../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md`](../designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md) (the Trunk integration design, which already names a mono switch as a config flip), [`../runbooks/flunge-trunk-enforcement-flip.md`](../runbooks/flunge-trunk-enforcement-flip.md), [`../runbooks/trunk-merge-queue-pre-enforcement-validation.md`](../runbooks/trunk-merge-queue-pre-enforcement-validation.md), [`buildkite-log-access-trunk-merge-2026-07-19.md`](buildkite-log-access-trunk-merge-2026-07-19.md), [`../../../checkleft/docs/designs/robust-change-detection-in-checkleft.md`](../../../checkleft/docs/designs/robust-change-detection-in-checkleft.md).
- **Evidence labels:** every claim below is marked **[verified]** (read in this checkout at the cited line, or quoted from the brief's read-only GitHub/flunge facts) or **[inference]** (a conclusion drawn from verified facts, or a property of Trunk/GitHub that was not observed directly).

## TL;DR

**Boss the product needs no engine, app or CLI change** to drive mono through Trunk, with one condition: mono must be reachable with the single org-level Trunk API token Boss already stores. If `spinyfin/mono` can be added to the same Trunk org as flunge, the switch is exactly the config flip the design promised. If Trunk forces mono into a second Trunk org, Boss needs multi-org token support first, which is a real engine change (the design's deferred task 14). Which of the two applies is the owner's first open question and cannot be settled from the repo.

**The mono repo needs one small code PR and some docs.** No `lib.sh`-style CI shim is needed for the diff base: checkleft already classifies a `trunk-merge/pr-<N>/<uuid>` build as a push to a non-default branch and diffs it against `merge-base(origin/main, HEAD)`, fetching `origin/main` itself on a shallow checkout. What checkleft cannot do today is recover the PR number from a Trunk branch name, so the PR-description surface (bypass directives and the `boss-ism/pr-text-leakage` scan) is silently "not applicable" on the gating build. Teaching checkleft the `trunk-*/pr-<N>/` shape generically is a ~50-line change plus tests. `.buildkite/pipeline.yml` needs no change. `REQUIRED_CHECKS.md`, `.buildkite/README.md` and the Trunk design doc need updating, and a mono cutover runbook should be written.

**Everything else is operator work:** install the Trunk GitHub App on the `spinyfin` org, add the repo and a `main` queue in Trunk with the three `buildkite/mono/*` statuses required, replace mono's "Merge Queue" ruleset with a Trunk-bypass update restriction, drain the GitHub queue, then flip the product with `boss product set-merge-mechanism`. Rollback is symmetric and the engine does not care which direction it goes.

## 1. Boss the product: does switching mono need a change?

### 1.1 Trunk org and token scope

**[verified]** Boss stores exactly one Trunk token, org-level, with no per-product dimension:

- `tools/boss/trunk_auth/src/lib.rs:22-23` fixes the keychain coordinates to one service/account pair (`dev.spinyfin.boss.trunk` / `api-token@trunk.io`); `:19` is the single env override `BOSS_TRUNK_API_TOKEN`.
- `tools/boss/protocol/src/types/product.rs:217` is the only Trunk-related product column, `merge_mechanism: Option<String>`. There is no per-product token, org slug or queue id.
- `tools/boss/app-macos/Sources/Settings/TrunkSettingsPane.swift:33-35` states the contract to the operator: "Org-level — one token covers every Trunk-queue product."
- `tools/boss/trunk_client/src/models.rs:14-17` shows every request is addressed by `TrunkRepoRef { host, owner, name }` plus `target_branch` (`:46-49`, `:65-67`, `:199-202`, `:247-249`). No request names a Trunk org; the org is implied by the bearer token.
- `tools/boss/engine/core/src/trunk_merge.rs:206-213` derives `(owner, repo, number)` from the task's PR URL, and `:226-232` builds the `TrunkRepoRef` from an `owner/name` slug with host pinned to `github.com` (`:18`). For mono that yields `spinyfin` / `mono`, with nothing flunge-specific. The only `brianduff/flunge` strings in the engine are test fixtures (`trunk_merge.rs:240-260`, `work/trunk_merge_intents.rs:365`).
- The design records this explicitly: multi-org token support is task 14, "future / not a v1 blocker", and the non-goals section says the mono prerequisites are "multi-org token support (task 14) if mono lands in a separate Trunk org" (`tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md:37`, and the task list under "Implementation task breakdown").

**[inference]** Therefore the product-change question reduces to one Trunk-side fact: whether `spinyfin/mono` (owned by a GitHub organisation) can be added to the existing Trunk org whose slug is `flunge` (whose repo is owned by the `brianduff` user account). Trunk orgs are linked to GitHub App installations, and a single Trunk org can hold repos from more than one installation in the general case, but this was not observed and the Trunk web app could not be read. If the answer is yes, no product change. If Trunk requires a second org, the token store, the `TrunkTokenProvider` wiring on `ServerState`, the `TrunkStatus`/`TrunkSetToken` RPCs (`tools/boss/engine/core/src/app/trunk_auth.rs:51-127`) and the Settings pane all need a per-org or per-product key; that is a design change, not a tweak, and it should be its own task before any cutover.

### 1.2 The hardcoded `main` target

**[verified]** `tools/boss/engine/core/src/merge_mechanism.rs:17` fixes `DEFAULT_TRUNK_TARGET_BRANCH = "main"` and `:41-43` parses `"trunk_queue"` into `TrunkQueue { target_branch: "main" }`. mono's default branch is `main` (the workspace's `main` bookmark tracks `main@origin`; the ruleset targets `~DEFAULT_BRANCH`; `.buildkite/pipeline.yml:41` gates releases on `build.branch == "main"`). No change needed.

### 1.3 CI-fix and eviction flows with mono's three per-step statuses

**[verified]** The eviction path does not depend on the shape of a repo's required checks:

- Eviction is detected from Trunk's own state via the queue poller (`tools/boss/engine/core/src/ci_watch.rs:1601-1660`, `on_trunk_queue_eviction_detected`), keyed on `trunk:<entry-id>@<stateChangedAt>` (`:1595-1597`). It is not derived from GitHub statuses.
- Trunk's bookkeeping check on the PR head, `Trunk Merge Queue (<branch>)`, is excluded from the required-failure rollup (`tools/boss/engine/core/src/merge_poller/classify.rs:351-372`, prefix constant at `:463`) so an eviction produces exactly one remediation. This is name-based and repo-independent.
- Failure evidence is an org-wide Buildkite search for the newest failed or failing build whose branch starts with `trunk-merge/pr-<N>/`, up to three pages, deliberately excluding `trunk-temp/*` (`tools/boss/engine/ci-log-reader/src/lib.rs:480-534`). It then emits one `RequiredCheckFailure` per failed job in that build (`tools/boss/engine/core/src/ci_watch.rs:1742-1756`). A three-step mono build yields up to three records with per-job log URLs, which is strictly more useful than flunge's single combined step, not less.
- mono's pipelines live in the same Buildkite org as flunge's, slug `flunge` (`.buildkite/README.md:72` links `buildkite.com/flunge/mono-checkleft-release/...`; `tools/boss/docs/buildkite-release-setup.md:115` links `buildkite.com/flunge/mono/builds/42`). The `bk api /builds` search is org-wide and `bk` is one binary (`tools/boss/engine/core/src/trunk_queue_poller.rs:1723`, `:1804`), so the evidence lookup covers mono without configuration.
- The worker-facing eviction prompt (`tools/boss/engine/core/src/runner/prompt.rs:2125-2142`, `:2197-2260`, `:2435-2460`) talks about "the failing job" on the construction branch and gives `bk` commands by branch prefix; nothing in it assumes one job per build.

**[inference]** One operational difference: with three required statuses, Trunk waits for all three on the construction branch before merging, and a flake in any one of them is an eviction that consumes a `ci_attempt_budget` slot (the design's acknowledged v1 limitation under "Risks / open questions": flake-flavoured evictions consume budget). The engine behaves the same as on flunge; the exposure is just three times wider per PR.

### 1.4 The GitHub-native queue paths go dormant correctly

**[verified]** Every GitHub-native queue path is gated on the mechanism, so flipping mono to `trunk_queue` disables them without code changes:

- `tools/boss/engine/core/src/merge_poller/merge_queue.rs:185-206` filters `trunk_queue` products out of the rebounce pass (`is_trunk_queue_product`, defined at `:520-525`).
- `tools/boss/engine/core/src/merge_poller/sweep.rs:2284` preserves Trunk-written `merge_queue_state`/`merge_queue_detail` columns so the GitHub probe's `mergeQueueEntry` recomputation (`merge_poller/probe.rs:346-350`) cannot wipe them.
- `tools/boss/engine/core/src/trunk_queue_adopt.rs:123-124` declines to adopt a Trunk episode on a non-`trunk_queue` product, so a product that has not been flipped yet cannot be half-adopted.
- `tools/boss/engine/core/src/merge_mechanism.rs:21-29` documents that `Direct` "transparently covers repos with a GitHub-native merge queue"; this is the mode mono runs in today (the brief: the mono product has no `merge_mechanism` set, and `:40` parses `None` to `Direct`). The `gh-readonly-queue` rebounce machinery (`tools/boss/github/src/check_runs.rs:121-124`, `tools/boss/github/src/gh_runner.rs:199-265`, `runner/prompt.rs:2120`, `:2411`) simply stops firing for mono once it is on `trunk_queue`. It stays in the engine for any future `Direct` product and needs no edit.
- `tools/boss/engine/core/src/conflict_watch.rs:646-668` and `:1654-1663` already coordinate conflict detection with a live Trunk intent for any product.
- `tools/boss/bossctl/src/doctor.rs:1332-1345` already classifies both `merge_queue_rebounce` and `trunk_queue_eviction` queue-side failures.

### 1.5 Misconfiguration backstop

**[verified]** If the ruleset is flipped before the product is, the `Direct` path's `gh pr merge --auto --squash` fails with a push-restriction error, which `tools/boss/engine/core/src/merge_mechanism.rs:68-98` recognises and turns into a `direct_merge_push_restriction` attention item naming the fix. If the product is flipped before the ruleset, Trunk accepts the submit but cannot update `main` while the native `merge_queue` rule is still active, and the engine surfaces that as an eviction or a stuck `not_ready` entry rather than a silent merge. Either ordering mistake fails loudly, but the correct order (ruleset first, then product) avoids both. See §4.

### 1.6 Small product-side gaps worth knowing (none blocking)

- **[verified]** `boss engine trunk status` smoke-checks only the first `trunk_queue` product it finds (`tools/boss/engine/core/src/app/trunk_auth.rs:142-150`, `products.into_iter().find(...)`). After the flip, "green" status proves the token works against flunge, not necessarily that mono's queue is reachable. Validation of mono must come from a real merge (§4), not from `trunk status`. A follow-up could make the smoke check iterate every `trunk_queue` product; it is a nice-to-have.
- **[verified]** The ruleset-level required-check contract still runs on the PR head via the GitHub probe (`merge_poller/classify.rs:372-381`, `isRequired` defaulting to true for legacy status contexts). mono's three contexts are legacy commit statuses, so they are treated as required on the PR head, which is what Trunk's readiness gate needs. No change.

**Verdict for §1: no change to Boss the product is required, conditional on §1.1.** If a second Trunk org is unavoidable, file "multi-org Trunk token support" as a prerequisite engine task; its file set is the two auth crates, `app/trunk_auth.rs`, `ServerState`, and `TrunkSettingsPane.swift`.

## 2. mono repo changes

### 2.1 What a `trunk-merge/pr-<N>/<uuid>` build looks like to mono's CI today

**[verified from the brief's flunge facts]** Trunk pushes `trunk-merge/pr-<N>/<uuid>` (gating), `trunk-temp/pr-<N>/<uuid>` (assembly, non-gating) and `trunk-conflict-investigation/pr-<N>/<uuid>/<sha>` branches and triggers each as a plain branch build. `BUILDKITE_PULL_REQUEST` is not a PR number on these builds (flunge's `trunk-temp`/`trunk-merge` builds have `pull_request: null`), and the checkout can be a shallow single-commit one.

**[verified]** mono's three gating steps have no branch filter and post their statuses through `notify: github_commit_status` with contexts `buildkite/mono/{bazel-build-test,mac-app-build,checks}` (`.buildkite/pipeline.yml:8-37`). Those statuses will be posted on the construction-branch commit exactly as they are on any branch push, which is what Trunk reads. The `boss-release` step is gated on `build.branch == "main"` plus a manual/scheduled source (`:41`) so it never runs on a Trunk branch. None of the three step scripts reads `BUILDKITE_PULL_REQUEST`, `BUILDKITE_BRANCH`, or computes a base ref (grep over `.buildkite/steps/*.sh` found no such reference; `checks.sh:23` is just `bin/checkleft run`, and `ci-env.sh` is Bazel startup plumbing). mono therefore has no `lib.sh` equivalent and, for the diff base, does not need one.

### 2.2 checkleft: scenario classification and base selection on a Trunk branch

**[verified]** On a `trunk-merge/pr-<N>/<uuid>` Buildkite build checkleft classifies as follows (`tools/checkleft/src/change_detection/scenario.rs:39-94`):

1. Rule 1 (`merge_group`) and rule 3 (`pull_request`) do not apply: no GitHub Actions.
2. Rule 2 does not apply: `is_merge_queue_branch` only matches the `gh-readonly-queue/` prefix (`merge_queue.rs:3-7`).
3. Rule 4 does not apply: `BUILDKITE_PULL_REQUEST` is `"false"`.
4. Rule 6 applies: `buildkite && buildkite_pull_request == "false"` with a non-default branch yields `Scenario::PushToBranch { branch }` (`scenario.rs:80-90`).

**[verified]** For `PushToBranch` the base is `merge-base(origin/<default_branch>, HEAD)`, falling back to the local branch name only if `origin/main` does not resolve (`base.rs:274-285`). Before that, `resolve_change_plan` sets `needed_ref = origin/main` (`mod.rs:135-140`) and `ensure_history` first does an explicit `git fetch --no-tags --depth=1 origin main:refs/remotes/origin/main` (`shallow.rs:67-70`, `:190-193`), then walks the deepen ladder `50 → 250 → 1000 → --unshallow` until the merge-base resolves (`shallow.rs:13`, `:99-100`, `:204-212`). If the base is still unreachable it fails loudly with `ChangesetUndetermined` rather than running an empty check set (`mod.rs:143-171`). This is the same treatment flunge gets from its hand-written `detect_base_ref()` (`git fetch --depth=200` plus `merge-base origin/main`), but it is already inside the shared tool. The e2e tests `push_to_branch_shallow_fetches_base_and_scopes_to_changed_files` and `push_to_branch_shallow_nonexistent_default_branch_errors_loudly` (`tools/checkleft/tests/change_detection_e2e.rs:922`, `:966`) cover exactly the shallow single-commit case.

**[inference]** Is `merge-base(origin/main, HEAD)` the correct base on Trunk's construction branch? The construction commit is the PR's changes applied on top of the `main` tip Trunk is targeting (as a merge or a squash; the shape was not observed). In the unbatched case `merge-base(origin/main, HEAD)` is that `main` tip whether or not `main` has moved since, so the diff is exactly the PR's changes. If Trunk batches entries so that PR N's construction branch is built on top of PR N-1's not-yet-merged construction commit, the merge-base is still the `main` tip and the diff also includes PR N-1's changes: over-inclusive, never under-inclusive, and never silently empty. That is the same behaviour flunge's bash produces and is acceptable for a changed-files gate. The `MergeQueue` row's `HEAD^1` rule (`base.rs:292-317`) must **not** be extended to Trunk branches: it is only correct when HEAD is known to be a merge commit whose first parent is the target tip, which is a GitHub `gh-readonly-queue` property that Trunk has not been observed to guarantee. Recommendation: leave Trunk builds as `PushToBranch`, and confirm on the first real construction-branch build that the `checks` step log shows `classified CI scenario` as `PushToBranch` (`mod.rs:128`; the plan summary at `main.rs:393` prints the row name) and a plausible changed-file count. If `BUILDKITE_PULL_REQUEST` turned out to be entirely absent rather than `"false"`, classification would fall to `Local` (`scenario.rs:93`), whose base is a working-tree merge-base against a local `main` that a Buildkite checkout may not have; this is contrary to Buildkite's documented semantics (the variable is always present, `false` when not a PR) and to mono's existing worker-branch builds, which take the same rule 6 path today, so it is listed only as the thing to look for in that first log.

### 2.3 checkleft: PR-number recovery is the real gap

**[verified]** checkleft resolves the PR description (used for bypass directives and the `changeset` surface of `boss-ism/pr-text-leakage`, `CHECKS.yaml:133`, `:157-165`) through a ladder in `tools/checkleft/src/main.rs`:

1. Explicit `CHECKS_CHANGE_ID` / `CHECKS_PR_NUMBER` env vars (`:248-249`, `:1759-1768`). This is the hook flunge's `run_checks.sh` uses, exporting `CHECKS_PR_NUMBER="$(pr_number_for_checks)"`.
2. Level 3a: a `gh-readonly-queue/<target>/pr-<N>-...` branch encodes the number directly (`:1903-1927`, parsed by `merge_queue.rs:23-41`).
3. Level 3b: branch-to-PR lookup via the GitHub API on `BUILDKITE_BRANCH` (`:1929-1958`), which explicitly skips `gh-readonly-queue/` branches (`:1977-1985`).

On a `trunk-merge/pr-<N>/<uuid>` build in mono: level 1 is unset, level 3a does not match, and level 3b searches GitHub for an open PR whose head branch is `trunk-merge/pr-<N>/<uuid>`, finds none, and returns `NotApplicable` (`:1944`), which is reported as "PR description surface: not applicable (no open PR for this change)" (`:1833-1835`). It is not an error, so the step does not fail, but on the gating build:

- bypass directives placed only in the PR description are invisible (`tools/checkleft/userdoc/docs/bypass.md:131-132` documents that the merge-queue build is exactly the context where branch-to-PR lookup finds nothing, which is why level 3a exists for GitHub's queue); and
- the `pr-text-leakage` scan of the PR body does not run on that build.

**[inference]** The practical impact is small, because the PR-head branch build still resolves the PR via level 3b and scans the description, and Trunk requires that head build to be green before admitting the PR (readiness). The exposure is a PR whose description changes after the head build, or a change that relied on a description-only bypass directive: that build would fail on the construction branch and evict the PR. `CHECKS.yaml:46-55` already tells authors to prefer the commit description for exactly this reason. Still, the fix is cheap and is the only code change this migration needs in mono.

**Recommended change (generic, in checkleft, not a mono-only shim):**

- Add a Trunk branch parser next to the GitHub one, either in `tools/checkleft/src/change_detection/merge_queue.rs` or a sibling `trunk_branch.rs`: match `^trunk-[^/]+/pr-(\d+)/` (the same regex flunge's `trunk_merge_pr_number()` uses, so all Trunk flavours resolve) and return the PR number.
- Use it at level 3a in `main.rs:1903-1927` (`pr_numbers_for_description`) so the description is fetched by number, and in `branch_from_ci_env` (`:1981-1985`) so level 3b does not waste an API call searching for a branch that is never a PR head.
- Extend `resolve_ref_for_upload` (`:2126-2163`) so the SARIF ref for a Trunk branch is `refs/heads/trunk-merge/...` the same way it is for `gh-readonly-queue`. This is harmless today (the ref is ephemeral) and keeps the two queues symmetric.
- Do **not** add a `Scenario` variant for Trunk; keep `PushToBranch` and its merge-base rule (§2.2).
- Tests: unit tests beside the parser, a `scenario.rs` test asserting a `trunk-merge/pr-42/<uuid>` Buildkite branch classifies as `PushToBranch`, and a `tests.rs` case parallel to the `gh-readonly-queue` one at `tools/checkleft/src/tests.rs:579` for PR-number recovery.
- Docs: `tools/checkleft/userdoc/docs/bypass.md:131-132` (add the Trunk shape to level 4), and a short note in `tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md` that Trunk construction branches are deliberately row 5, not row 3.

Why generic rather than a mono shim: checkleft is a shared tool consumed by flunge via prebuilt releases (`tools/checkleft/docs/buildkite-release-setup.md`), flunge currently carries its own bash for this, and the GitHub-queue equivalent already lives in checkleft. A mono-only `ci-env.sh` export of `CHECKS_PR_NUMBER` would work (five lines mirroring flunge's `trunk_merge_pr_number`) but would be the second copy of the same regex in two repos. If the owner wants the smallest possible mono diff before cutover, the shim is an acceptable interim and the generic change can follow.

### 2.4 `.buildkite/pipeline.yml`

**[verified]** No change is required. The three gating steps build every branch and post the three statuses (§2.1). Two optional adjustments, both operator decisions rather than requirements:

- **[inference]** A Buildkite branch filter excluding `trunk-temp/*` would avoid building Trunk's assembly branches, which do not gate (flunge evidence: a `trunk-temp` build failed while the `trunk-merge` build passed and the PR merged). The flunge example shows three `trunk-temp` builds for one `trunk-merge` build, and mono's `mac-app-build` step runs on the `macos-arm64` queue, which is the scarcer fleet (`.buildkite/README.md:61-63`). Whether Trunk needs statuses on `trunk-temp/*` or `trunk-conflict-investigation/*` for its own conflict probing was not observed; flunge builds them all. Recommendation: start by building everything (parity with the validated flunge setup) and add the filter only if `macos-arm64` backs up.
- The Buildkite pipeline setting that requests builds for GitHub `merge_group` events can be turned off after cutover, since GitHub will no longer emit them. It is harmless left on.

### 2.5 `REQUIRED_CHECKS.md` and other docs

- `.buildkite/REQUIRED_CHECKS.md:3-5`, `:13`, `:44` describe the three contexts as a branch-protection contract. After cutover the same three names are also Trunk's required statuses on the construction branch, so a rename has three places to update, not two. Add a "Trunk merge queue" paragraph and a rename-contract step. While there, fix the stale path at `:50` (`tools/boss/engine/src/ci_watch.rs` should be `tools/boss/engine/core/src/ci_watch.rs`), which the design doc lists as open follow-up.
- `.buildkite/README.md:89-91` ("Required checks (branch protection)"): add the Trunk-side requirement.
- `tools/boss/docs/designs/trunk-merge-queue-integration-queue-backed-merges-merging-ui.md:37` says migrating mono is a non-goal "via the existing `gh pr merge --auto` verb"; update it to point at this investigation and the mono runbook once the flip happens.
- New runbook `tools/boss/docs/runbooks/mono-trunk-cutover.md`: the §4 sequence below, adapted from the flunge flip and validation runbooks, which are written for `brianduff/flunge` throughout.
- `tools/cube/src/app/repo.rs:578` mentions `gh-readonly-queue/*` refs in a comment about ref pruning only; no change.

## 3. GitHub and Trunk settings (operator steps)

These require admin on the `spinyfin` GitHub org and the Trunk web app, so they are operator-only. Nothing here is something a worker can or should do.

### 3.1 Trunk

1. Install the Trunk.io GitHub App (app id 120106, slug `trunk-io`) on the `spinyfin` organisation, granting it `spinyfin/mono`. **[inference]** The same app is what flunge's ruleset exempts, so the identity Trunk merges with will be the same bot.
2. In the Trunk web app, add `spinyfin/mono` to the existing `flunge` Trunk org if Trunk allows a repo from a different GitHub owner in that org (§1.1). If it does not, stop and file the multi-org token task before continuing.
3. Create a merge queue on `main` for mono. Set the required statuses to exactly `buildkite/mono/bazel-build-test`, `buildkite/mono/mac-app-build`, `buildkite/mono/checks`, and the merge method to squash (mono's ruleset today allows squash only). Batching, concurrency and timeouts: flunge's values could not be read; start with Trunk's defaults, and note that mono's `bazel-build-test` plus `mac-app-build` wall time is what bounds queue throughput, not Trunk's settings.
4. Generate (or reuse) the org-level API token. If the token is reused, nothing changes in Boss. If a new token is issued for any reason, `boss engine trunk set-token` once, then `boss engine trunk status`.

### 3.2 GitHub rulesets and branch protection on `spinyfin/mono`

Current state (from the brief): ruleset "Merge Queue" on `~DEFAULT_BRANCH` with `deletion`, `non_fast_forward`, `merge_queue` (squash, ALLGREEN, 5/5, 60-minute check timeout, 5-minute wait) and `pull_request` (squash only, 0 approvals), no bypass actors; classic protection requiring the three contexts with `strict: false`.

Target state, mirroring flunge's verified configuration:

1. **Remove the `merge_queue` rule** from the ruleset (or delete that ruleset and create a replacement). This is what turns the native queue off. Do it only after the queue is drained (§4).
2. **Add an `update` rule** (restrict updates to `main`) alongside the existing `deletion` and `non_fast_forward` rules, with the Trunk.io app as the sole exempt bypass actor. This is flunge's "main - force push" shape and is the actual enforcement: only Trunk can move `main`.
3. **Keep the `pull_request` rule** (squash only, 0 approvals). Trunk merges through the PR, so this is compatible, and it keeps humans from pushing commits to `main` directly even if the update restriction is later loosened.
4. **Keep the three required status checks** on the PR head with `strict: false`. "Require branches to be up to date" must stay off: the queue owns freshness (`tools/boss/docs/runbooks/flunge-trunk-enforcement-flip.md`, step 2 of the flip sequence).
5. **Keep the ruleset scoped to `~DEFAULT_BRANCH` only**, so `trunk-merge/*`, `trunk-temp/*` and `trunk-conflict-investigation/*` are unencumbered (flip runbook step 3). This is already the case.
6. Repo merge settings: squash allowed is required; allow auto-merge and delete-branch-on-merge match flunge and are unaffected either way.

Verify with `gh api repos/spinyfin/mono/rules/branches/main` and `gh api repos/spinyfin/mono/branches/main/protection` before and after.

### 3.3 Boss

Only after §3.1 and §3.2 are done and the GitHub queue is empty:

```sh
boss product set-merge-mechanism <mono product slug> --mechanism trunk_queue
boss engine trunk status
```

(`tools/boss/cli/src/commands.rs:346-347`.) Remember that `trunk status` probes only the first `trunk_queue` product (§1.6); the real check for mono is the first merge in §4.

## 4. Cutover and rollback

### 4.1 Ordering

1. **Land the mono repo PR(s) from §6 first.** The checkleft change is inert until a `trunk-*` branch is built, and the docs need to be in place for whoever runs the next steps.
2. **Trunk side (§3.1).** Inert while the native `merge_queue` rule still owns `main`: Trunk can run CI on construction branches but cannot update `main`. **[inference]** Do not submit anything to the Trunk queue in this window; a submit would sit `not_ready` or be evicted when Trunk's merge is rejected.
3. **Pre-flight the CI shape without Trunk.** Push a throwaway branch named `trunk-merge/pr-<N>/preflight` (where `<N>` is a real open PR) to mono. Buildkite builds every branch, so this exercises §2.2 and §2.3 for free: check the `checks` step log for `classified CI scenario` = `PushToBranch`, a sane changed-file count, the PR-description status line (`scanned` once the checkleft change has landed, `not applicable` before it), and that all three `buildkite/mono/*` statuses land on the commit. Delete the branch afterwards.
4. **Freeze merges in Boss and drain the GitHub queue.** Stop clicking merge; let anything already `QUEUED` merge or, if the stall persists, remove the entries from the queue in the GitHub UI. **[inference]** Removing the `merge_queue` rule while entries are queued dequeues them; draining first keeps that from being a surprise. On the Boss side, each dequeue is observed by the poller and clears the row's queue columns (`tools/boss/engine/core/src/merge_poller/merge_queue.rs:213-280`, `clear_github_merge_intent_on_observed_dequeue`). Wait until no mono card is in the Merging lane before step 5: once the product is on `trunk_queue`, the GitHub probe stops rewriting those columns (§1.4), so a card that was still "queued" at flip time would be stuck there until its PR merged or closed.
5. **Replace the ruleset (§3.2).**
6. **Flip the product (§3.3).**
7. **Validate,** adapting `tools/boss/docs/runbooks/trunk-merge-queue-pre-enforcement-validation.md` to mono. Step 1 (one real merge through the Boss merge button: `trunk_enqueued`, card in Merging, badge progresses, GitHub-side `merged` observed, task to Done) is the minimum. Step 2 (one deliberate eviction) is strongly recommended because mono's three-status shape has never been exercised through the eviction evidence path; use the corrected recipe from the design's risks section: the scratch PR must pass its own CI but fail against `main` on the construction branch, otherwise Trunk never admits it. Confirm the `ci-fix` revision's prompt cites a `buildkite.com/flunge/mono/builds/<n>#<job>` URL, which proves the org-wide evidence search (§1.3) found the mono build.
8. **Re-enable merging.** PRs that were dequeued in step 4 need one merge click each in Boss to enter the Trunk queue.

### 4.2 Rollback

Symmetric, and the engine assumes nothing about which direction it moves (design: "nothing in the engine assumes enforcement"; `tools/boss/docs/runbooks/flunge-trunk-enforcement-flip.md`, "Rollback"):

1. Pause the mono queue in the Trunk web app so it does not race the next steps.
2. `boss product set-merge-mechanism <mono product slug> --mechanism direct`.
3. Restore the ruleset: remove the `update` rule and its Trunk bypass, re-add the `merge_queue` rule with the previous parameters. (If GitHub's queue is the thing that is broken, the interim is `direct` with no queue rule at all, which is flunge's pre-enforcement state: Boss's `gh pr merge --auto --squash` lands PRs as soon as the three required checks are green.)
4. Active `trunk_merge_intents` rows for mono stop being submitted to; a PR Trunk had already merged is detected GitHub-side as usual, and a cancelled entry retires its intent with an attention item (design, "Humans and Boss sharing the queue"). No DB cleanup.
5. The Trunk app installation, the Trunk repo, and the checkleft change can all stay; they are inert without the ruleset.

## 5. Risks and open questions for the owner

1. **Trunk org membership (§1.1).** Can `spinyfin/mono` join the `flunge` Trunk org, or does Trunk force a second org? This is the only question that can turn "no product change" into "engine change first". Answerable in the Trunk web app in minutes; answer it before anything else.
2. **Trunk's construction-commit shape and batching (§2.2).** `merge-base(origin/main, HEAD)` is correct for a single entry and over-inclusive for a batched one. Over-inclusion is safe for a gate but can produce `change/file-count` or `changed_lines_only` findings attributed to the wrong PR if Trunk batches. Observe on the first real queue build; if batching is on and this bites, the remedy is a Trunk-side setting (disable batching) rather than a checkleft rule.
3. **`BUILDKITE_PULL_REQUEST` on Trunk builds (§2.2).** Expected `"false"` (rule 6, `PushToBranch`). If it is ever absent, checkleft falls to `Local`. The pre-flight branch in §4.1 step 3 settles this before Trunk is involved.
4. **Three statuses, one eviction budget (§1.3).** A flake in any of the three steps on the construction branch spends a `ci_attempt_budget` slot. mono already lives with flake exposure on the native queue; what changes is that recovery is automatic (Boss auto-resubmits) rather than a human re-adding the PR to the GitHub queue.
5. **`macos-arm64` load (§2.4).** Every Trunk branch flavour triggers `mac-app-build`. If the Mac fleet backs up, filter `trunk-temp/*` in Buildkite; whether Trunk needs those statuses was not observed.
6. **`trunk status` covers one product (§1.6).** Green does not prove mono is wired. Use the validation merge.
7. **PR-description surface on the gating build (§2.3).** Until the checkleft change lands, bypass directives and the leakage scan on the construction branch depend on the PR-head build having already run with the same description. Low practical risk; the fix is in §6.
8. **Shared bot identity and comments.** `trunk-io[bot]` will comment on every mono PR ("Merging to `main` is managed by Trunk … `/trunk merge`"), as it does on flunge. Workers must not run `/trunk merge` or `gh pr merge` themselves; the eviction prompt already says so, and the Boss-side worker rules in `.claude/CLAUDE.md` already forbid bare merges.
9. **GitHub support ticket.** If the native queue stall is a GitHub-side rulesets bug rather than a merge-queue bug, it may also affect the replacement ruleset's evaluation. Nothing in this plan depends on the native queue working, but keep the ticket open until a Trunk-driven merge has landed on mono.
10. **Rate limits.** `trunk_client` already honours `Retry-After` on 429 (`tools/boss/trunk_client/src/client.rs:141-160`); mono's PR volume is higher than flunge's but still a handful of requests per minute at Hot-tier cadence. Watch for `trunk_queue_unreachable` attention items in the first week.

## 6. Proposed implementation breakdown

Minimal shape: **one code PR, one docs PR (or one combined PR), then operator steps.** Nothing in Boss the product changes unless question 1 in §5 says a second Trunk org is required.

| #   | What                                                                                                                                                                                                                                    | Who                                                        | Forces the order                                                                                                           |
| --- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| 1   | checkleft: generic `trunk-*/pr-<N>/` PR-number recovery at level 3a, the level 3b branch filter, the SARIF ref, tests, `bypass.md` and change-detection design note (§2.3). No `Scenario` change.                                       | worker PR                                                  | Must land before the first construction-branch build for the PR-description surface to be scanned there. Inert until then. |
| 2   | Docs: `.buildkite/REQUIRED_CHECKS.md` (Trunk-side required statuses, rename contract, stale path fix), `.buildkite/README.md`, Trunk design doc non-goal note, new `tools/boss/docs/runbooks/mono-trunk-cutover.md` carrying §3 and §4. | worker PR (can be folded into #1)                          | Should exist before the operator runs §3/§4.                                                                               |
| 3   | Trunk app install on `spinyfin`, repo added to the Trunk org, `main` queue with the three required statuses, token confirmed (§3.1).                                                                                                    | operator                                                   | After the org-membership question is answered; independent of #1/#2.                                                       |
| 4   | Pre-flight branch build (§4.1 step 3).                                                                                                                                                                                                  | operator (or a worker, with the branch deleted afterwards) | After #1 for a `scanned` description line; useful even before it.                                                          |
| 5   | Freeze and drain the GitHub queue; confirm no mono card is in Merging (§4.1 step 4).                                                                                                                                                    | operator                                                   | Immediately before #6.                                                                                                     |
| 6   | Ruleset replacement (§3.2).                                                                                                                                                                                                             | operator                                                   | After #5; before #7.                                                                                                       |
| 7   | `boss product set-merge-mechanism <mono> --mechanism trunk_queue`; `boss engine trunk status` (§3.3).                                                                                                                                   | operator                                                   | After #6, or Trunk submits will be rejected at merge time.                                                                 |
| 8   | Validation: one real merge, one deliberate eviction (§4.1 step 7).                                                                                                                                                                      | operator, human-in-the-loop                                | After #7; gates re-enabling merges.                                                                                        |
| 9   | Optional follow-ups: `trunk status` smoke check iterating every `trunk_queue` product; Buildkite `trunk-temp/*` filter; multi-org Trunk token support only if §5 question 1 requires it.                                                | worker tasks, filed separately                             | None are prerequisites unless question 1 says otherwise.                                                                   |

Follow-up work to file separately, if the owner agrees: the checkleft change (#1), the docs/runbook PR (#2), and the optional `trunk status` improvement (#9).
