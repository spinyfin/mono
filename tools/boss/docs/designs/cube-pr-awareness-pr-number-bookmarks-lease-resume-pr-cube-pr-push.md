# Cube PR-awareness: pr-number bookmarks, PR-head positioning, `cube pr push`

- **Status:** implemented, with one capability superseded after landing.
- **Design written:** 2026-06 (pre-implementation). **Revised:** 2026-07-20 to describe as-built reality.
- **Shipped by:** #1374 (T1), #1379 (T2), #1381 (T3), #1380 + #1390 (T4), #1382 (T5), #1391 (Boss-WI-A).
- **Superseded after landing:** the `cube workspace lease --resume_pr <n>` flag this design proposed shipped in #1380, then was removed as dead code in #1560 and replaced by a separate positioning verb, `cube workspace goto --pr <n>` (see _Positioning on a PR head_). The filename still carries the original `lease-resume-pr` title; the capability it names no longer exists under that name.
- **Not delivered:** Boss-WI-B — the revision worker still advances its parent PR with raw `jj bookmark set` + `cube pr update` rather than `cube pr push`, so `cube pr push` currently has no in-repo consumer.

## Verdict

The two-bookmark model and the `pr/<n>` reserved namespace shipped essentially as designed and are load-bearing today. The third capability, `cube pr push`, also shipped as designed — but nothing calls it, because the boss-side adoption item (Boss-WI-B) was never done. The second capability, PR-head positioning, shipped as a lease flag and was then re-cut as a standalone `cube workspace goto` verb: the same job, moved out of the lease path, which turned out to be the better factoring.

## Problem

Boss leans on cube to own the _workspace + PR bookkeeping contract_: cube leases a reusable jj workspace, positions the working copy, and (via `cube pr ensure`) pushes a branch and opens or reuses a GitHub PR. But cube's PR-awareness stopped at "create or reuse a PR for the current bookmark." It had no concept of _an existing PR you want to land another commit on_, addressed _by its PR number_.

That gap is exactly what revision workers need. The revision-tasks design (`revision-tasks.md`, T1363/PR #1364) added a task kind whose deliverable is _a new commit on the parent task's existing PR branch_ — no new PR. Before this project, the engine hand-rolled the jj choreography for that in the worker spawn prelude: a raw `jj git fetch` → `jj edit <PARENT_BRANCH>` → `jj new @` → edit → `jj bookmark set <PARENT_BRANCH>` → `jj git push -b <PARENT_BRANCH>` block (`revision-tasks.md` Q3). Every other PR concern — remote-name disambiguation (the local-mirror trap), push verification against GitHub's truth, idempotency — already lived in cube and was _re-implemented, badly, or skipped_ when boss reached around cube to drive jj directly.

This design added three cube capabilities so boss can lean on cube for "advance an existing PR by number" the same way it already leans on cube for "open a PR":

1. **pr-number bookmarks** — after `cube pr ensure` pushes, it also sets a **local-only** jj bookmark named for the actual PR number (`pr/<n>`) on the pushed commit, so a workspace can later find "the commit for PR N" by number without branch-name archaeology.
2. **PR-head positioning** — put a workspace's working copy on a fresh empty commit on top of PR N's head, ready to edit, with no branch hunting. Designed as `cube workspace lease --resume_pr <n>`; as-built this is `cube workspace goto --pr <n>`.
3. **`cube pr push`** — advance both the remote head branch _and_ the local `pr/<n>` bookmark to the current commit and push, so the new commit lands on the **existing** PR (never a new one).

The original deliverable was **this design doc only**, with the boss-side rewire sketched as follow-up. That follow-up was partially done: Boss-WI-A landed (#1391) and was subsequently rewired again onto `cube workspace goto`; Boss-WI-B did not land.

## Goals

- **`cube pr ensure` sets a local-only `pr/<n>` bookmark** on the pushed commit, named for the real PR number, as pure local bookkeeping. The bookmark is _never_ tracked or pushed to any remote. **Shipped as designed.**
- **A positioning primitive** that lands the workspace with `@` as a fresh _empty_ commit on top of PR N's head — editable immediately, no branch hunting. **Shipped, but as a standalone `cube workspace goto` verb rather than a lease flag.**
- **`cube pr push`** advances _both_ bookmarks (the remote-tracked head branch and the local `pr/<n>`) to the current commit and pushes the head branch to GitHub, landing the new commit on the existing PR. Fast-forward by default; `--force-with-lease` available as an explicit opt-in; never an accidental force; idempotent on a no-op. **Shipped as designed; no consumer yet.**
- **Cube stays the owner of the workspace/PR-bookkeeping contract.** The pr-number-bookmark convention, positioning, and "advance an existing PR" verb are generic git/PR plumbing — legitimately cube's domain. **Held.**
- **No boss-specific policy in cube.** "Which PR a revision targets," soft-prefer fallback policy, and completion semantics stay in boss. Cube exposes mechanism; boss supplies policy. **Held.**
- **No regression** to existing `cube pr ensure` / `cube workspace lease` behavior. **Held**, with one caveat: the push guard means `cube pr ensure --branch pr/<n>` now errors where it previously would have pushed (intended, and nothing did this).

## Non-goals

- **Stack-aware PR push.** `cube pr push` advances _one_ PR's head. Stacked-PR rebasing already lives in `cube stack rebase` / `cube pr sync`; this design does not extend it. Still out of scope.
- **Implicit force-push.** `cube pr push` never force-pushes implicitly. An explicit `--force-with-lease` flag is required for any rewrite scenario; it shipped in v1 as a guarded opt-in (see _Fast-forward vs force_).
- **Mirroring the pr-number → workspace mapping into cube's SQLite store.** The bookmark lives in jj, not in cube's registry (see _Where the bookmark lives_). No new store table/column was added.
- **Auto-rebasing onto a moved `main`.** If GitHub's PR head advanced from elsewhere, `cube pr push` fails loudly and the caller rebases the normal way. No new rebase machinery.
- **Changing how `cube pr ensure` discovers `owner/repo`, the github remote, or verifies the push.** Those mechanisms (`parse_github_remote`, `verify_push_reached_github`) are reused verbatim.
- **A new GitHub-comment or review surface.** Out of scope; this is plumbing only.

## Model

There are **two bookmark roles** that can sit on a worker's PR commit in a cube workspace. Keeping their responsibilities crisp is the heart of this design, and this part survived implementation unchanged.

| Bookmark               | Example             | Tracked to a remote?          | Keyed by                | Source of truth for                                                                                                      |
| ---------------------- | ------------------- | ----------------------------- | ----------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| **Remote head branch** | `boss/exec_18b6…_b` | **Yes** — `…@<github-remote>` | engine-supplied exec id | What is _on the PR_ (this is the PR's GitHub head ref). Authoritative for reviewers and CI.                              |
| **PR-number bookmark** | `pr/1364`           | **No** — local only           | the real PR number      | _Which local commit corresponds to PR N in this workspace_ — a convenience pointer, reconciled against GitHub on demand. |

Key relationships:

- After `cube pr ensure`, `cube workspace goto --pr <n>`, or `cube pr push`, **both bookmarks point at the same commit**. They are two names for the same head: one is the remote ref GitHub knows, the other is a local handle keyed by the durable, human/engine-resolvable PR number.

- **GitHub is the ultimate source of truth** for the PR head. The local `pr/<n>` is a cache that can go stale if something else advances the PR from another workspace, so every consumer reconciles against GitHub before trusting it — but _how_ they reconcile diverged from the plan. The design said each primitive would `jj git fetch` first. As built, `cube workspace goto` does fetch and then resolves the head from the fetched remote-tracking ref; **`cube pr push` never fetches**, and instead reads GitHub's head sha directly via `gh api repos/<owner>/<repo>/branches/<branch> --jq .commit.sha`. Both reconcile against GitHub's truth; only one does it by fetching.

- The **PR number is the durable cross-session identifier**; the `boss/exec_*` head branch is an engine artifact whose name a _resuming_ worker may not know. `pr/<n>` is what lets "resume PR 1364" resolve without knowing the exec-id branch name.

- **The local fast path was never built.** The design pitched `pr/<n>` as a zero-network answer to "where is PR N's head locally," with a `gh` fallback only when the bookmark was absent. Neither `cube workspace goto` nor `cube pr push` has such a fast path: both always consult GitHub (`gh pr view` / `gh api`). `pr/<n>` is therefore not a network-avoidance cache in practice — it is a durable local _handle_ that lets `cube pr push` infer which PR the current commit belongs to without being told. That is still valuable, but it is a different justification than the one originally written down.

### pr-number bookmark naming/format

**`pr/<n>`** — the literal prefix `pr/` followed by the decimal PR number (e.g. `pr/1364`). jj accepts `/` in bookmark names (it is how the existing `boss/exec_*` bookmarks are named), and the `pr/` namespace is otherwise unused. The prefix is a **reserved cube namespace**: cube treats any bookmark matching `pr/<digits>` as managed local bookkeeping.

The matcher shipped without a regex dependency — it strips the `pr/` prefix and requires a non-empty all-ASCII-digit remainder, which gives `^pr/\d+$` semantics and correctly rejects `pr/`, `pr/1a`, `pr/1/2`, and `xpr/1`.

### How `pr/<n>` is _guaranteed_ to stay local

A jj bookmark only reaches a remote in two ways: (a) it is named explicitly in a `jj git push -b <name>` (or swept up by `jj git push --all` / `--tracked`), or (b) it is a _tracking_ bookmark bound to a remote (`jj bookmark track <name>@<remote>`). The guarantee rests on cube owning every push in this contract and never doing either:

1. **Cube never names `pr/*` in a push.** `cube pr ensure` and `cube pr push` push the _head branch_ by explicit `-b <head-branch>`. They never pass `pr/<n>` to `jj git push`, and cube never uses `jj git push --all`/`--tracked`.
2. **Cube never tracks `pr/<n>` to a remote.** No remote ever has a `pr/<n>` ref, so there is nothing to track; cube issues no `jj bookmark track pr/*`.
3. **Belt-and-suspenders push guard.** Cube's push paths assert that the bookmark being pushed does not match the reserved `pr/<digits>` pattern, turning a silent leak into a loud error.

This is _namespace + invariant_, not a jj-level "private bookmark" feature (jj has no first-class local-only bookmark flag). The guarantee is enforced by cube's discipline plus the guard.

The honest boundary: cube guarantees that _cube's own_ pushes never carry `pr/*`. A worker that hand-runs `jj git push -b pr/5` is out of contract and cannot be guarded against from inside cube.

### Where the helper lives

The `pr/<n>` helpers (`is_pr_bookmark`, `pr_bookmark_name`, `assert_not_pr_bookmark`) live in the shared **`lib/rust/git_utils`** crate (`src/pr_bookmark.rs`), not inside cube. The design originally placed them in "a small helper module in cube"; they were factored out into the shared crate so the namespace rule has one home for any consumer, consistent with the repo's prefer-crates-over-modules convention. Cube depends on `git-utils` and calls through `pr_bookmark::`.

### Where the bookmark lives — per-workspace jj, not cube's store

The `pr/<n>` bookmark is a **jj local bookmark in the per-workspace jj repo**. It is _not_ a row in cube's SQLite registry.

Rationale: the bookmark must travel with the commit and branch _inside the workspace_ — it is git/jj state, not lease state. Putting a `(pr_number → workspace_id, commit_sha)` row in cube's store would create a second source of truth that immediately drifts the moment jj rewrites or advances the commit, and it would have to be invalidated on every reset, fetch, and rebase. Cube's store stays the registry of _workspaces and leases_; jj stays the registry of _commits and bookmarks_. One fact, one home.

A consequence worth stating: because `pr/<n>` lives in one workspace's jj repo, a _different_ workspace does not automatically know about PR N. That is fine and intended — positioning recovers the head from GitHub regardless of whether the local bookmark is present.

## Alternatives considered

### Alternative A — Keep hand-rolling jj in the boss engine directive (status quo)

Leave cube unchanged; continue driving the revision positioning and push as a raw-jj block in the worker spawn prelude (`revision-tasks.md` Q3).

**Why not:** It re-implements — or silently skips — the hard parts cube already solved. The spawn-prelude block did a bare `jj git push -b <PARENT_BRANCH>` with no github-remote disambiguation, so it was exposed to the local-mirror trap that `cube pr ensure` exists to defeat. It did no push verification against GitHub's truth. It resolved the parent branch by name, which a resuming worker must be _told_, where a PR number is the more durable handle the operator actually has.

**Outcome:** rejected for positioning — #1391 deleted the raw-jj positioning block in favour of a cube call, and that deletion has stuck across the later re-cut onto `cube workspace goto`. But A is still the status quo for the _push_ half: the revision worker directive continues to run raw `jj bookmark set` before `cube pr update` (see _Outstanding work_).

### Alternative B — Track the pr-number → commit mapping in cube's SQLite store, no jj bookmark

Add a `pr_bookmarks(repo, pr_number, workspace_id, commit_sha)` table; `cube pr ensure` writes a row; the positioning and push commands read it to find the commit.

**Why not:** It makes cube's store a second source of truth for git state that jj already owns, and the two drift constantly — every `jj git fetch`, rebase, amend, or reset moves the commit out from under the stored sha. You would need cache-invalidation hooks on operations cube does not even mediate. The jj-bookmark approach gets correctness for free: the bookmark _is_ a jj ref, so jj keeps it consistent with the commit graph. Rejected, and nothing in implementation challenged that call.

### Alternative C — Reuse the existing `boss/exec_*` head branch as the only handle; resolve "PR N" via `gh` every time

No new bookmark at all. To position on PR N, always call `gh pr view <n> --json headRefName`, fetch that branch, and position on it.

**Why not (as designed):** it means a network round-trip to GitHub on every positioning and push, even in the warm-workspace case where the commit is already local. The plan was **C-as-fallback + the `pr/<n>` bookmark as a zero-network fast path**.

**What actually shipped is closer to plain C than to the plan.** `cube workspace goto --pr <n>` calls `gh pr view` unconditionally and sets `pr/<n>` from the fetched remote ref whether or not a local bookmark already existed; there is no branch on local-bookmark presence. The `pr/<n>` bookmark's surviving job is _inference_ — letting `cube pr push` work out which PR the current commit belongs to — not network avoidance. Given that positioning must reconcile against GitHub anyway (which the design itself required, even on the "fast" path), the fast path would have saved little; collapsing to one always-correct path is simpler and is the right call in hindsight, but the doc's original network-cost argument for the bookmark does not hold up.

### Alternative D — A jj `git.push-bookmark-prefix` / config-based local-only marker

Lean on a jj or git config knob to mark `pr/*` as un-pushable.

**Why not:** jj has no first-class "this bookmark is local-only" flag, and config-based prefixes govern _generated_ push names, not a deny-list. Relying on a config knob also moves the guarantee out of cube and into per-workspace jj config that a `jj git fetch`/reset could disturb. Rejected; the namespace + cube-side guard shipped instead.

## As-built design

### 1. `cube pr ensure` — set `pr/<n>` after the PR number is known

Flow (`app.rs` `ensure_pr`): resolve `(github_remote, owner_repo)` → **push guard** → `jj git push -b <branch> --remote <github_remote> --allow-new` → `verify_push_reached_github` → `gh pr list --head <branch> --state open` → reuse-or-create → set bookmark → return `{url, number}`.

Once the PR number `n` is known — whether the PR already existed or was just created (parsed via the existing `pr_number_from_url`) — cube sets the local bookmark on the pushed commit:

```
jj bookmark set pr/<n> -r <branch>
```

`jj bookmark set` is create-or-move and idempotent, so re-running `cube pr ensure` is safe.

**Ordering (load-bearing):** push → ensure/discover PR number → set bookmark. The bookmark is only set _after_ a PR number exists, so if `gh pr create` fails there is no number and no bookmark (correct — there is no PR to bookmark), and if the PR already existed the number is discovered and the bookmark is (re)set, which **backfills** `pr/<n>` for branches pushed before this feature shipped with no special-casing.

**Multiple-PR guard:** the `gh pr list` call requests `--state open`, and more than one open PR for the head is a hard error rather than a guess. (One open PR per head is the invariant boss relies on.) Note the call requests `--json url` only — the number is still derived from the URL by the pre-existing parser, not requested from `gh` as the design assumed.

**Push guard:** before `jj git push`, cube asserts the resolved branch name does not match the reserved `pr/<digits>` pattern and refuses loudly if it does. This is the local-only guarantee's enforcement point. It landed in T1 alongside the helper rather than in T2 as planned, which means T1 was not the pure no-behavior-change substrate the plan described — `cube pr ensure --branch pr/42` began erroring as of #1374.

**Two behaviors the design did not specify, settled during implementation:**

- _Unparseable PR number is a soft no-op._ If the number cannot be parsed from the PR URL, no bookmark is set, `pr_bookmark` is `null` in the payload, and `ensure_pr` still succeeds. The PR is real and pushed; losing a local convenience bookmark should not fail the command.
- _A failed `jj bookmark set` is fatal._ If the bookmark set itself fails, `ensure_pr` errors even though the push and PR creation already succeeded. This is stricter than the design's "side effect, optionally noted" framing implied — the bookmark is treated as part of the command's contract, not best-effort.

The printed stdout contract is unchanged (the PR URL stays the only stdout line). The structured `RunResult` JSON always carries a `"pr_bookmark"` field, which is the bookmark name or `null` — always present rather than "optionally noted."

### 2. Positioning on a PR head — `cube workspace goto --pr <n>`

**This is the capability that changed shape after landing.** It shipped first as a lease flag and now exists as a standalone verb.

**What was designed:** `cube workspace lease --resume_pr <n>` — a flag that left workspace _selection_ unchanged and replaced only the post-claim positioning step (the normal `jj new <main>` reset) with "land on PR N's head."

**What shipped in #1380:** exactly that, with the flag spelled `--resume-pr` (clap kebab-cases the long name; the doc's `--resume_pr` was never the CLI spelling). It composed with `--prefer`, conflicted with `--allow-dirty` in both directions, surfaced a nested `resume_pr: {pr_number, head_branch}` object in the lease JSON, and rolled the lease back on positioning failure.

**What #1390 then fixed:** #1380's resume path replaced `reset_workspace_guarded` wholesale and, with it, silently dropped the `prior_expired` dirty guard. On a workspace reclaimed from an _expired_ lease with uncommitted work in `@`, `jj new pr/<n>` would have snapshotted the previous holder's files into the fresh commit on top of the PR head — silently contaminating the PR. #1390 ported the `is_clean_on_main` check into the resume path, refusing with `LeaseExpiredWorkspaceDirty` (exit 7) and rolling the lease back to free.

That bug is the clearest evidence for why the flag was the wrong factoring. Positioning-inside-lease had to re-derive, and initially failed to re-derive, guard behavior that the normal lease path already owned. The design's failure-mode table asserted the resume path "does not engage" the dirty-recovery path; implementation proved that assertion wrong within an hour of the flag landing.

**What exists today:** #1560 removed the `--resume-pr` flag from `cube workspace lease` as dead code, along with `PrResumeInfo` and `resume_workspace_on_pr`. Positioning is now a separate verb:

```
cube workspace goto [--workspace <path>] (--bookmark <name> | --pr <n>)
```

`--bookmark` and `--pr` are mutually exclusive; one is required. The `--pr` sequence (`workspace_goto` in `app.rs`) is:

1. Resolve the github remote (`jj git remote list` → `parse_github_remote`).
2. `jj git fetch --remote <github_remote>`.
3. `gh pr view <n> -R <owner_repo> --json headRefName,state` — hard error if `MERGED` or `CLOSED`, hard error if no `headRefName`.
4. Verify the remote-tracking ref `<branch>@<remote>` exists locally; error with a remediation hint if not.
5. `jj bookmark set <branch> -r <branch>@<remote> --allow-backwards` — a plain local bookmark forced to match the remote, not a `jj bookmark track` as the design specified.
6. `jj bookmark set pr/<n> -r <branch>@<remote> --allow-backwards`, so `cube pr push` and `cube workspace rebase` can recover the PR number later.
7. `jj new` on the remote ref — skipped if `@` already has it as a direct parent (idempotent).

The reconcile target is the **fetched remote-tracking ref**, not the `headRefOid` the design named. `headRefOid` is not requested by `goto` at all.

**Why the re-cut is the better design.** Separating positioning from leasing means the lease path keeps exactly one reset/guard implementation, and positioning becomes idempotent and independently re-runnable against a workspace you already hold — which is what the engine actually wanted. Selection, dirty-recovery, and expiry handling all stay in `lease`; `goto` is a pure positioning verb. The cost is that boss now makes two calls where the design imagined one.

**State after `cube workspace goto --pr <n>`:** `@` is a fresh empty commit; `pr/<n>` and the head branch both point at PR N's current head (the parent of `@`); the working copy is ready to edit. This is what a revision worker needs (T1363), same as designed.

### 3. `cube pr push`

`cube pr push` advances an existing PR with the current commit — the "land another commit on an existing PR" counterpart to `cube pr ensure`'s "create or first-push." It shipped in #1381 as designed, with a broader resolution surface.

**Resolving which PR/branch to advance** — four cases, where the design anticipated two:

- `--pr <n> --branch <b>`: both given, used directly.
- `--pr <n>` alone: resolve the head branch from the non-`pr/*` bookmark co-located with `pr/<n>`; error with a `cube workspace goto --pr <n>` hint if the bookmark is absent.
- `--branch <b>` alone: resolve the number via `gh pr list --head <b> --state open --json number`.
- Neither: infer from `@`'s ancestry — the nearest ancestor (or `@` itself) carrying a `pr/<n>` bookmark. If none is found, error instructing the caller to pass `--pr`.

**Sequence:**

1. Resolve remote and target; apply the `pr/*` push guard to the head branch.
2. `check_pr_open` — `gh pr view <n> --json state`; hard error unless `OPEN`. This runs _before_ the empty/descendant guards.
3. Empty-`@` check: no-op success if `pr/<n>` already matches GitHub's head sha, otherwise refuse (nothing to land).
4. Advance both bookmarks to `@` (`jj bookmark set <head-branch> -r @`, `jj bookmark set pr/<n> -r @`), fast-forward only.
5. `jj git push -b <head-branch> --remote <github_remote>` — **no `--allow-new`** (the branch already exists remotely; `--allow-new` would mask a "branch vanished" surprise).
6. `verify_push_reached_github` — assert GitHub's head sha equals local `@`.
7. **Never push `pr/<n>`** — it stays local.

The structured payload reports `{"action": "pushed"|"noop", "url", "number"}`.

**Reconciliation is API-based, not fetch-based.** `pr push` does not `jj git fetch`. It reads GitHub's head sha via `gh api repos/<owner>/<repo>/branches/<branch> --jq .commit.sha` for the no-op and lease checks. This is a deliberate simplification — the command only needs one sha, not a full fetch — but it means the Model section's "every consumer fetches first" claim describes `goto` only.

**Fast-forward vs force.** The default push is fast-forward only: `@` must be a descendant of `pr/<n>`. If GitHub's head moved since, the push is rejected as stale, and cube surfaces that loudly with an instruction to fetch and rebase. Cube **never** passes `--force`/`-f` implicitly.

`cube pr push --force-with-lease` is the guarded opt-in for legitimate rewrites. Two implementation details worth recording: it shells out to raw `git push --force-with-lease <remote> <branch>` because `jj git push` has no `--force-with-lease` flag, and it **skips the descendant check entirely** — lease verification (jj's `<branch>@<remote>` tracking sha equals GitHub's sha) is the only safety on that path. That is the correct semantic for a rewrite, but it is a real widening of what the flag permits versus the "fast-forward plus a lease check" the design implied.

**Fast-forward verification checks `pr/<n>` only,** not "each bookmark's prior target" as designed. The head-branch bookmark's prior position is not independently checked; jj's own refusal to move a bookmark backwards without `--allow-backwards` is the implicit backstop.

**Idempotency.** Re-running with no new commit is a no-op, short-circuiting at the empty-`@` check (step 3) rather than at the verify step as the design described.

### Failure modes

| Failure                                      | Where                            | Behavior                                                                                                                                                 |
| -------------------------------------------- | -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PR number not found (`gh pr view <n>` fails) | `workspace goto --pr`, `pr push` | Error: cannot resolve PR N's head; surface gh's message. No positioning/push performed.                                                                  |
| >1 **open** PR for a head branch             | `cube pr ensure`                 | Error rather than guessing which PR to bookmark. **Not** enforced on `cube pr push --branch <b>`, which takes the first result (see _Outstanding work_). |
| Dirty working copy on an expired lease       | `cube workspace lease`           | Refused with `LeaseExpiredWorkspaceDirty` (exit 7); lease rolled back to free. Now uniform, since positioning left the lease path.                       |
| Empty `@`                                    | `cube pr push`                   | No-op success if `pr/<n>` already matches GitHub's head; otherwise refuse (nothing to land).                                                             |
| Detached / non-descendant `@`                | `cube pr push`                   | Refuse; instruct the caller to reposition with `cube workspace goto --pr <n>` and rebuild on the head. Skipped under `--force-with-lease`.               |
| Push rejected (stale remote head)            | `cube pr push`                   | Loud error; instruct `jj git fetch` + rebase. Never auto-force.                                                                                          |
| PR is MERGED/CLOSED                          | `workspace goto --pr`, `pr push` | Hard error in both cases; `--json state` is checked before any positioning or pushing.                                                                   |
| Attempt to push a `pr/*` bookmark            | `cube pr ensure`, `cube pr push` | Refused by the push guard — the local-only invariant's enforcement point.                                                                                |
| `pr/<n>` absent locally                      | `workspace goto --pr`            | Not an error and not a special case: `goto` always resolves the head from GitHub and creates the bookmark.                                               |
| `pr/<n>` absent locally                      | `cube pr push --pr <n>`          | Error with a hint to run `cube workspace goto --pr <n>` first or pass `--branch`.                                                                        |

## Boss integration — as built

The boss revision flow (T1363) was **half** rewired onto these primitives.

**Positioning: done, then re-done.** #1391 (Boss-WI-A) threaded `resume_pr: Option<u64>` through the `CubeClient` and `HostAdapter` traits, extracted the PR number from `execution.pr_url`, and passed `--resume-pr <n>` on both lease attempts (preferred and fallback), so cube's GitHub-resolution path recovered the head when the warm workspace was unavailable. It deleted `HostAdapter::position_revision_workspace` and the ~60-line raw-jj `position_revision_workspace_local` helper, and set a `revision_positioned_by_lease` flag so the coordinator skips `create_change`. A positioning failure became `cube_workspace_lease_failed` (no workspace acquired, nothing to release), replacing the old post-lease `revision_pr_positioning_failed`.

When the lease flag was withdrawn (#1560), the coordinator moved to a post-lease `cube workspace goto --workspace <path> --pr <n>` call. #1522 additionally fixed the directive falsely claiming pre-positioning when `pr_url` was absent. The raw-jj positioning block has stayed deleted throughout — that part of Boss-WI-A's intent held even though its mechanism did not.

**Push: not done.** Boss-WI-B was never delivered. The revision worker directive still instructs the worker to run `jj bookmark set <parent-branch-name> -r @` and then `cube pr update --branch <parent-branch-name>` (`runner.rs:2532`, `runner.rs:2535`). `cube pr push` has no consumer anywhere in `tools/boss` — the only occurrence of the string is in this document.

The practical consequence is that revision workers still hand-advance the parent bookmark. `cube pr update` does give them cube's remote disambiguation and push verification, so this is materially safer than the original raw `jj git push` tail; what they lose relative to `cube pr push` is the open-PR check, the descendant/fast-forward guard, the empty-`@` no-op, and the coupled advancement of `pr/<n>`.

**Completion detection is unchanged.** The detector still confirms the parent PR head advanced and matches the chain root's `pr_url` (`revision-tasks.md` Q4). Neither `cube pr update` nor `cube pr push` opens a new PR, so the no-new-PR invariant holds either way.

**Constraint check:** none of the cube primitives encode "revision = commit on parent PR." `pr/<n>`, positioning-by-number, and advance-existing-PR are generic. The revision semantics stay in boss. ✔

## Risks / open questions

- **R1 — Local-only guarantee depends on cube discipline, not a jj feature.** _Held._ jj has no first-class local-only bookmark; the guarantee is namespace + cube never pushing `pr/*` + the push guard. The guard turns any accidental `cube … --branch pr/<n>` into a loud error. A worker that hand-runs `jj git push -b pr/5` remains out of contract — an accepted boundary, not a closed question.

- **R2 — Stale `pr/<n>` after an external advance.** _Held, differently mechanized._ Every consumer reconciles against GitHub before trusting the local bookmark — `goto` by fetching and re-setting from the remote ref, `pr push` by comparing against GitHub's head sha over the API. The fast-forward-only rule turns a genuinely diverged state into a loud stale-push error rather than a silent clobber.

- **R3 — `cube pr push` PR-inference ambiguity.** _Settled as designed._ Ancestry inference is the default; `--pr`/`--branch` are explicit overrides, and either one alone resolves the other. If no `pr/<n>` ancestor is found, `cube pr push` errors instructing the caller to pass `--pr`.

- **R4 — Force-push policy.** _Settled, with a caveat worth knowing._ `--force-with-lease` shipped as an explicit, guarded opt-in; the default stays fast-forward-only and implicit force is never permitted. The caveat: the flag skips the descendant check entirely and relies solely on the lease comparison, so it permits more than "fast-forward plus a lease check."

- **R5 — Merged/closed PR handling.** _Settled as designed._ Hard error in both `cube workspace goto --pr` and `cube pr push`; `--json state` is checked and a `MERGED`/`CLOSED` result is refused with a clear message.

- **R6 — Backfill timing.** _Settled, and less relevant than expected._ `pr/<n>` is only set the next time a cube command touches the PR, but since no consumer depends on the bookmark being pre-present (`goto` always resolves from GitHub), backfill was a non-event.

- **R7 — Bookmark hygiene / GC.** _Closed._ Shipped in #1382 despite being marked optional; see T5.

- **New open question — should `cube pr push` fetch?** It reconciles via a single `gh api` sha read rather than `jj git fetch`. That is cheaper and sufficient for the guards it implements, but it means the local jj repo can hold a stale view of the remote branch while the command reports success. Whether that matters depends on whether a caller inspects local refs after a push; no current caller does, because there are no callers.

## Implementation task breakdown — as shipped

### T1 — `pr/<n>` reserved namespace, helper, and push guard — **shipped (#1374)**

Landed as `lib/rust/git_utils/src/pr_bookmark.rs` (shared crate, not a cube-internal module as planned) with `is_pr_bookmark`, `pr_bookmark_name`, and `assert_not_pr_bookmark`. The guard was wired into `ensure_pr` in this PR rather than deferred to T2, so T1 was not behavior-neutral as the plan claimed. Unit tests for the matcher and formatter, plus an `ensure_pr` guard integration test.

### T2 — `cube pr ensure` sets the local `pr/<n>` bookmark — **shipped (#1379)**

`jj bookmark set pr/<n> -r <branch>` on both the create and reuse/backfill paths; `gh pr list` tightened to `--state open` with a hard error on more than one open PR; `"pr_bookmark"` added to the `RunResult` payload (always present, nullable). Stdout contract unchanged. Tests cover create, reuse/backfill, and the multi-open-PR error.

### T3 — `cube pr push` subcommand — **shipped (#1381), no consumer**

`PrCommand::Push` with `--pr`, `--branch`, and `--force-with-lease`. Full resolution matrix, open-PR check, empty/detached guards, fast-forward advancement of both bookmarks, push without `--allow-new`, and push verification. Ten app tests plus three clap tests. The primitive is complete and unused — see _Outstanding work_.

### T4 — PR-head positioning — **shipped (#1380, #1390), then superseded (#1560)**

Shipped as `cube workspace lease --resume-pr <n>`; #1390 added the `prior_expired` dirty guard the flag had bypassed; #1560 removed the flag as dead code once the engine moved to `cube workspace goto --pr <n>`. The capability survives; the surface named in this doc's title does not.

### T5 — Bookmark GC for closed-PR `pr/*` bookmarks — **shipped (#1382)**

Marked `future / not a v1 blocker` in the plan, shipped anyway. `gc_collect_closed_pr_bookmarks` hooks into `gc_workspace_bookmarks`, so it runs on release, pool GC, and `cube workspace gc`: list `pr/*` bookmarks via `jj log -r 'bookmarks(glob:"pr/*")'`, resolve each PR's state with `gh pr view`, forget `MERGED`/`CLOSED`, retain `OPEN`. All failures are swallowed (offline is a skip, not an error), and one shared `jj bookmark forget` covers both the exec and pr sweeps. Four new tests.

One cost the plan did not anticipate: state resolution is a serial `gh pr view` per bookmark, so a workspace with many accumulated `pr/*` bookmarks pays N network round-trips per GC sweep.

### Boss-WI-A — Coordinator positions revision workspaces via cube — **shipped (#1391), rewired (#1522, #1560)**

Delivered as `--resume-pr` on the lease call, later re-pointed at `cube workspace goto --pr <n>` as a post-lease step. The raw-jj positioning block is gone and has stayed gone.

### Boss-WI-B — Revision worker directive calls `cube pr push` — **not delivered**

The worker directive still emits `jj bookmark set <parent-branch-name> -r @` + `cube pr update --branch <parent-branch-name>` (`runner.rs:2532`, `runner.rs:2535`). This is the one planned item with no PR behind it.

## Outstanding work

Two gaps this review surfaced, both filed as follow-up tasks:

1. **Boss-WI-B is undelivered, leaving `cube pr push` with zero consumers.** The revision worker still hand-advances the parent bookmark with raw `jj bookmark set` before `cube pr update`. Adopting `cube pr push` would give revision pushes the open-PR check, descendant guard, empty-`@` no-op, and coupled `pr/<n>` advancement that the standalone primitive already implements and tests.

2. **`cube pr push --branch <b>` does not enforce the one-open-PR-per-head invariant.** The `(None, Some(branch))` arm of `resolve_pr_push_target` takes `prs.first()` from `gh pr list --head <b> --state open`, silently picking one when several match — the exact case `cube pr ensure` was tightened to reject in T2.
