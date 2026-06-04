# Cube PR-awareness: pr-number bookmarks, `lease --resume_pr`, `cube pr push`

## Problem

Boss leans on cube to own the *workspace + PR bookkeeping contract*: cube leases a reusable jj workspace, positions the working copy, and (via `cube pr ensure`) pushes a branch and opens or reuses a GitHub PR. But cube's PR-awareness stops at "create or reuse a PR for the current bookmark." It has no concept of *an existing PR you want to land another commit on*, addressed *by its PR number*.

That gap is exactly what revision workers need. The revision-tasks design (`revision-tasks.md`, T1363/PR #1364) added a task kind whose deliverable is *a new commit on the parent task's existing PR branch* — no new PR. Today the engine hand-rolls the jj choreography for that in the worker spawn prelude: a raw `jj git fetch` → `jj edit <PARENT_BRANCH>` → `jj new @` → edit → `jj bookmark set <PARENT_BRANCH>` → `jj git push -b <PARENT_BRANCH>` block (`revision-tasks.md` Q3). Every other PR concern — remote-name disambiguation (the local-mirror trap), push verification against GitHub's truth, idempotency — already lives in cube and is *re-implemented, badly, or skipped* when boss reaches around cube to drive jj directly.

This design adds three cube capabilities so boss can lean on cube for "advance an existing PR by number" the same way it already leans on cube for "open a PR":

1. **pr-number bookmarks** — after `cube pr ensure` pushes, it also sets a **local-only** jj bookmark named for the actual PR number (`pr/<n>`) on the pushed commit, so a workspace can later find "the commit for PR N" by number without branch-name archaeology.
2. **`cube workspace lease --resume_pr <n>`** — position a freshly-leased workspace so the working copy is a fresh empty commit *on top of PR N's head*, ready to edit, with no branch hunting.
3. **`cube pr push`** — advance both the remote head branch *and* the local `pr/<n>` bookmark to the current commit and push, so the new commit lands on the **existing** PR (never a new one).

The deliverable here is **this design doc only**. The boss-side rewire (replacing the raw-jj spawn prelude with calls to these primitives) is a follow-up, sketched but explicitly out of scope for implementation.

## Goals

- **`cube pr ensure` sets a local-only `pr/<n>` bookmark** on the pushed commit, named for the real PR number, as pure local bookkeeping. The bookmark is *never* tracked or pushed to any remote.
- **`cube workspace lease --resume_pr <n>`** lands the leased workspace with `@` as a fresh *empty* commit on top of PR N's head — editable immediately, no branch hunting — composing with the existing `--prefer` warm-workspace machinery rather than duplicating it.
- **`cube pr push`** advances *both* bookmarks (the remote-tracked head branch and the local `pr/<n>`) to the current commit and pushes the head branch to GitHub, landing the new commit on the existing PR. Fast-forward only; never an accidental force; idempotent on a no-op.
- **Cube stays the owner of the workspace/PR-bookkeeping contract.** The pr-number-bookmark convention, resume positioning, and "advance an existing PR" verb are generic git/PR plumbing — legitimately cube's domain.
- **No boss-specific policy in cube.** "Which PR a revision targets," soft-prefer fallback policy, and completion semantics stay in boss. Cube exposes mechanism; boss supplies policy.
- **No regression** to existing `cube pr ensure` / `cube workspace lease` behavior. The new bookmark is additive; the new flag and subcommand are opt-in.

## Non-goals

- **Implementing the boss-side rewire.** The integration is sketched (see *Boss-integration sketch*) and the implied boss work items are listed, but no boss code (`runner.rs`, `coordinator.rs`, etc.) is touched by this project. `future / not a v1 blocker`.
- **Stack-aware PR push.** `cube pr push` advances *one* PR's head. Stacked-PR rebasing already lives in `cube stack rebase` / `cube pr sync`; this design does not extend it. `future / not a v1 blocker`.
- **A force-push escape hatch.** v1 `cube pr push` is fast-forward only. A `--force-with-lease`-style override is deferred (see Risks). Never an *implicit* force.
- **Mirroring the pr-number → workspace mapping into cube's SQLite store.** The bookmark lives in jj, not in cube's registry (see *Where the bookmark lives*). No new store table/column.
- **Auto-rebasing onto a moved `main`.** If GitHub's PR head advanced from elsewhere between fetch and push, `cube pr push` fails loudly and the caller rebases the normal way. No new rebase machinery.
- **Changing how `cube pr ensure` discovers `owner/repo`, the github remote, or verifies the push.** Those mechanisms (`parse_github_remote`, `verify_push_reached_github`) are reused verbatim.
- **A new GitHub-comment or review surface.** Out of scope; this is plumbing only.

## Model

There are now **two bookmark roles** that can sit on a worker's PR commit in a cube workspace. Keeping their responsibilities crisp is the heart of this design.

| Bookmark | Example | Tracked to a remote? | Keyed by | Source of truth for |
| --- | --- | --- | --- | --- |
| **Remote head branch** | `boss/exec_18b6…_b` | **Yes** — `…@<github-remote>` | engine-supplied exec id | What is *on the PR* (this is the PR's GitHub head ref). Authoritative for reviewers and CI. |
| **PR-number bookmark** | `pr/1364` | **No** — local only | the real PR number | *Which local commit corresponds to PR N in this workspace* — a convenience pointer, reconciled against GitHub on demand. |

Key relationships:

- After `cube pr ensure` or `cube pr push`, **both bookmarks point at the same commit**. They are two names for the same head: one is the remote ref GitHub knows, the other is a local handle keyed by the durable, human/engine-resolvable PR number.
- **GitHub is the ultimate source of truth** for the PR head. The local `pr/<n>` is a cache: it can go stale if something else advances the PR from another workspace. Every primitive that reads it (`lease --resume_pr`, `cube pr push`) first `jj git fetch`es and reconciles against GitHub before trusting it. This mirrors the existing principle that "the branch state is recoverable from GitHub via `jj git fetch`; warmth is an optimization only" (`revision-tasks.md` OQ5).
- The **PR number is the durable cross-session identifier**; the `boss/exec_*` head branch is an engine artifact whose name a *resuming* worker may not know. `pr/<n>` is what lets "resume PR 1364" resolve without knowing the exec-id branch name.

### pr-number bookmark naming/format

**`pr/<n>`** — the literal prefix `pr/` followed by the decimal PR number (e.g. `pr/1364`). jj accepts `/` in bookmark names (it is how the existing `boss/exec_*` bookmarks are named), and the `pr/` namespace is otherwise unused. The prefix is a **reserved cube namespace**: cube treats any bookmark matching `pr/<digits>` as managed local bookkeeping.

### How `pr/<n>` is *guaranteed* to stay local

A jj bookmark only reaches a remote in two ways: (a) it is named explicitly in a `jj git push -b <name>` (or swept up by `jj git push --all` / `--tracked`), or (b) it is a *tracking* bookmark bound to a remote (`jj bookmark track <name>@<remote>`). The guarantee rests on cube owning every push in this contract and never doing either:

1. **Cube never names `pr/*` in a push.** Both `cube pr ensure` and `cube pr push` push the *head branch* by explicit `-b <head-branch>`. They never pass `pr/<n>` to `jj git push`, and cube never uses `jj git push --all`/`--tracked` (it pushes one named bookmark at a time today, and this design keeps that).
2. **Cube never tracks `pr/<n>` to a remote.** No remote ever has a `pr/<n>` ref, so there is nothing to track; cube issues no `jj bookmark track pr/*`.
3. **Belt-and-suspenders push guard.** Cube's push paths gain a cheap assertion: refuse to push any bookmark whose name matches the reserved `pr/<digits>` pattern. This catches a future caller (or a boss directive bug) that tries to `cube pr ensure --branch pr/1364`, turning a silent leak into a loud error.

This is *namespace + invariant*, not a jj-level "private bookmark" feature (jj has no first-class local-only bookmark flag). The guarantee is enforced by cube's discipline plus the guard, which is exactly the kind of contract cube already enforces around the local-mirror remote trap.

### Where the bookmark lives — per-workspace jj, not cube's store

The `pr/<n>` bookmark is a **jj local bookmark in the per-workspace jj repo**. It is *not* a row in cube's SQLite registry.

Rationale: the bookmark must travel with the commit and branch *inside the workspace* — it is git/jj state, not lease state. Putting a `(pr_number → workspace_id, commit_sha)` row in cube's store would create a second source of truth that immediately drifts the moment jj rewrites or advances the commit, and it would have to be invalidated on every reset, fetch, and rebase. Cube's store stays the registry of *workspaces and leases*; jj stays the registry of *commits and bookmarks*. One fact, one home. (This is the same separation that already keeps `head_commit` as a *cached* hint in the store while jj remains authoritative.)

A consequence worth stating: because `pr/<n>` lives in one workspace's jj repo, a *different* workspace does not automatically know about PR N. That is fine and intended — `lease --resume_pr` recovers the head from GitHub when the local bookmark is absent (see below), and the warm-workspace preference is the optimization that usually makes it already-present.

## Alternatives considered

### Alternative A — Keep hand-rolling jj in the boss engine directive (status quo)

Leave cube unchanged; continue driving the revision positioning and push as a raw-jj block in the worker spawn prelude (`revision-tasks.md` Q3).

**Why not:** It re-implements — or silently skips — the hard parts cube already solved. The spawn-prelude block does a bare `jj git push -b <PARENT_BRANCH>` with no github-remote disambiguation, so it is exposed to the local-mirror trap that `cube pr ensure` exists to defeat (`.claude/CLAUDE.md`, "The `origin` remote is a LOCAL MIRROR"). It does no push verification against GitHub's truth. It resolves the parent branch by name (`jj edit <PARENT_BRANCH>`), which a resuming worker must be *told*, where a PR number is the more durable handle the operator actually has. And it scatters PR bookkeeping across boss directives instead of concentrating it where the contract lives. The whole point of cube owning the workspace/PR contract is undermined every time boss reaches around it.

### Alternative B — Track the pr-number → commit mapping in cube's SQLite store, no jj bookmark

Add a `pr_bookmarks(repo, pr_number, workspace_id, commit_sha)` table; `cube pr ensure` writes a row; `lease --resume_pr` and `cube pr push` read it to find the commit.

**Why not:** It makes cube's store a second source of truth for git state that jj already owns, and the two drift constantly — every `jj git fetch`, rebase, amend, or reset moves the commit out from under the stored sha. You would need cache-invalidation hooks on operations cube does not even mediate (a worker rebases inside its own session). The jj-bookmark approach gets correctness for free: the bookmark *is* a jj ref, so jj keeps it consistent with the commit graph, and `jj git fetch` + reconcile handles staleness with machinery that already exists. A DB row buys nothing the bookmark does not, and costs an invalidation problem.

### Alternative C — Reuse the existing `boss/exec_*` head branch as the only handle; resolve "PR N" via `gh` every time

No new bookmark at all. To "resume PR N," always call `gh pr view <n> --json headRefName`, fetch that branch, and position on it.

**Why not:** It works, and in fact it is precisely the *fallback* path this design keeps for when `pr/<n>` is absent (see `--resume_pr` resolution). But making it the *only* path means a network round-trip to GitHub on every resume and push, even in the warm-workspace common case where the commit is already local. The `pr/<n>` bookmark is the local fast-path: present in the workspace that built the PR (the warm case boss already optimizes for), it answers "where is PR N's head locally" with zero network. C is the floor; the bookmark is the optimization layered on top. We adopt **C-as-fallback + the bookmark as fast-path**, which is the chosen approach.

### Alternative D — A jj `git.push-bookmark-prefix` / config-based local-only marker

Lean on a jj or git config knob to mark `pr/*` as un-pushable.

**Why not:** jj has no first-class "this bookmark is local-only" flag, and config-based prefixes (`git.push-bookmark-prefix`) govern *generated* push names, not a deny-list. Relying on a config knob also moves the guarantee out of cube and into per-workspace jj config that a `jj git fetch`/reset could disturb. The chosen *namespace + cube-side push guard* keeps the guarantee in code cube controls, which is more robust and self-documenting.

## Chosen approach

Adopt the two-bookmark model above and implement the three capabilities as additive cube surface. Each reuses existing cube primitives (`parse_github_remote`, `verify_push_reached_github`, the `--prefer` lease machinery) rather than re-deriving them.

### 1. `cube pr ensure` — set `pr/<n>` after the PR number is known

Current flow (`app.rs` `ensure_pr`): resolve `(github_remote, owner_repo)` → `jj git push -b <branch> --remote <github_remote> --allow-new` → `verify_push_reached_github` → `gh pr list --head <branch>` → reuse-or-create → return `{url, number}`.

**Change:** once the PR number `n` is known — whether the PR already existed (parsed from the `gh pr list` URL) or was just created (parsed from `gh pr create` output via the existing `pr_number_from_url`) — set the local bookmark on the pushed commit:

```
jj bookmark set pr/<n> -r <branch>
```

`jj bookmark set` is create-or-move and idempotent, so re-running `cube pr ensure` is safe. `-r <branch>` ties it to the same commit the head branch points at (which is `@` at this point, but naming the branch is unambiguous if `@` later moves).

**Ordering (load-bearing):** push → ensure/discover PR number → set bookmark. The bookmark is only set *after* a PR number exists, so:
- If `gh pr create` fails, no number, no bookmark — correct (there is no PR to bookmark).
- If the PR already existed, the number is discovered and the bookmark is (re)set — this **backfills** `pr/<n>` for branches pushed before this feature shipped, and for resumed workspaces, with no special-casing.

**Multiple-PR guard:** `gh pr list --head <branch>` can in principle return more than one row (e.g. a closed PR plus an open one on the same head). Today the code takes `.first()`. This design tightens it: request `--state open --json url,number`, and if more than one *open* PR is returned, error rather than guessing which to bookmark. (One open PR per head is the invariant boss relies on.)

**Push guard:** before the existing `jj git push`, assert the resolved branch name does not match the reserved `pr/<digits>` pattern; refuse loudly if it does. This is the local-only guarantee's enforcement point.

No change to the printed stdout contract (the PR URL stays the only stdout line); the bookmark is a side effect, optionally noted in the structured `RunResult` JSON (`"pr_bookmark": "pr/<n>"`).

### 2. `cube workspace lease --resume_pr <n>`

A new flag on `cube workspace lease`. Selection of *which* workspace to lease is unchanged — `--resume_pr` only changes the **post-claim positioning step**, replacing the normal `jj new <main>` reset with "land on PR N's head."

**Clap surface:**
- `--resume_pr <PR_NUMBER>` (integer).
- Composes with `--prefer <workspace-id>` for warmth (see below).
- **Mutually exclusive with `--allow-dirty`** (clap `conflicts_with`): `--allow-dirty` preserves a crashed worker's dirty tree as-is; `--resume_pr` repositions the working copy. Asking for both is contradictory.

**Positioning sequence** (replacing the `reset_workspace_guarded` → `jj new main` step for this lease only):

1. `jj git fetch --remote <github_remote>` — bring down the PR's current head. This is load-bearing for the *cold* workspace case where the branch is not present locally (same reasoning as `revision-tasks.md` "Why `jj git fetch` first is load-bearing").
2. Resolve PR N's head:
   - **Fast path:** if local `pr/<n>` exists, take it as the candidate head, but still reconcile against GitHub (step 3) to catch staleness.
   - **Fallback (Alternative C):** if `pr/<n>` is absent in this workspace, resolve the head branch from GitHub: `gh pr view <n> --json headRefName,headRefOid,state`. Fetch that ref (covered by step 1 once the branch name is known; fetch the specific branch if needed). Then create the local bookmark on the fetched head commit.
   - In both cases, end with a local `pr/<n>` bookmark pointing at the **current GitHub head commit** of PR N, and (re)establish the head-branch bookmark locally tracking `<head-branch>@<github_remote>` so a later `cube pr push` has a branch to advance.
3. **Reconcile:** set `pr/<n>` to the GitHub head oid (move it forward if the local copy was behind). The PR head commit is treated as **immutable** — we never rewrite it.
4. **Land editable:** `jj new pr/<n>` — create a fresh *empty* child commit on top of the (immutable) PR head and point `@` at it. The worker can edit immediately; the head it builds on is exactly PR N's current head.

Record the new `@` as the lease's `head_commit` (the existing `update_workspace_head_commit` call), and surface the resolved `{pr_number, head_branch}` in the lease JSON so the caller need not re-derive them.

**Cross-workspace resume — compose, don't duplicate.** `--resume_pr` is orthogonal to *which* workspace gets leased. The warm-workspace preference is the *existing* `--prefer` machinery: the caller passes `--resume_pr <n> --prefer <workspace-that-built-PR-N>`. If the preferred workspace is free it is leased and the fast path (local `pr/<n>` already present) hits; if it is gone or leased, the existing best-effort `--prefer` silently falls back to any free workspace and the fallback (gh + fetch) path recovers the head from GitHub. This reuses `lease_workspace_with_fallback`'s soft-prefer behavior wholesale — `--resume_pr` adds *no* new workspace-selection policy, only a positioning step that runs after a workspace is claimed.

**State after a `--resume_pr` lease:** `@` is a fresh empty commit; `pr/<n>` and the head branch both point at PR N's current head (the parent of `@`); the working copy is clean and ready to edit. This is precisely what a revision worker needs (T1363).

### 3. `cube pr push`

A new `cube pr push` subcommand: advance the existing PR with the current commit. It is the "land another commit on an existing PR" counterpart to `cube pr ensure`'s "create or first-push."

**Resolving which PR/branch to advance:**
- Default (no args): infer from `@`'s ancestry. Find the nearest ancestor (or `@` itself) carrying a `pr/<n>` bookmark; its `<n>` is the PR, and the co-located head branch is the ref to push. If none is found, error instructing the caller to use `--resume_pr` first or pass `--pr`/`--branch`.
- Explicit: `cube pr push --pr <n>` and/or `--branch <head-branch>` to disambiguate (e.g. when the inference is ambiguous, or the caller already knows N).

**Sequence:**

1. **Snapshot & guards.** jj auto-snapshots the working copy. Then:
   - **Empty `@` / no-op:** if `@` is empty *and* both bookmarks already point at the current GitHub head (nothing new to land), return success without pushing (idempotent no-op). This is the "ran `cube pr push` twice" case.
   - **Non-descendant / detached:** if `@` is *not* a descendant of the current `pr/<n>` target, refuse — pushing would either lose the PR's history or land an unrelated commit. Instruct the caller to `--resume_pr` and rebuild on the head.
2. **Advance both bookmarks** to `@`, fast-forward only:
   ```
   jj bookmark set <head-branch> -r @
   jj bookmark set pr/<n> -r @
   ```
   Verify the move is a fast-forward: `@` must be a descendant of each bookmark's prior target. (jj `bookmark set` moving backward requires `--allow-backwards`; we never pass it.)
3. **Push the head branch** to GitHub by name (reusing the github-remote resolution): `jj git push -b <head-branch> --remote <github_remote>` — **no `--allow-new`** (the branch already exists remotely; `--allow-new` would mask a "branch vanished" surprise). Apply the same `pr/*` push guard as `cube pr ensure` (the head branch is never a `pr/*` name; the guard catches a misuse).
4. **Verify** the push reached GitHub via the existing `verify_push_reached_github` (assert GitHub's head sha equals local `@`).
5. **Never push `pr/<n>`** — it stays local.

**Fast-forward vs force.** v1 pushes only fast-forwards. If GitHub's head moved since the last fetch (someone else advanced the PR), the push is **rejected as stale** by GitHub; cube surfaces that loudly and instructs the caller to `jj git fetch` and rebase. Cube **never** passes `--force`/`-f` implicitly. A guarded `--force-with-lease`-style override is deferred (Risks).

**Idempotency.** Re-running with no new commit is a no-op (step 1). Re-running after a successful push (bookmarks already at `@`, GitHub head already equals `@`) short-circuits at the verify step as success.

### Failure modes (all three commands)

| Failure | Where | Behavior |
| --- | --- | --- |
| PR number not found (`gh pr view <n>` fails) | `--resume_pr`, `cube pr push --pr` | Error: cannot resolve PR N's head; surface gh's message. No positioning/push performed. |
| >1 **open** PR for a head branch | `cube pr ensure` | Error rather than guessing which PR to bookmark. |
| Dirty working copy | `--resume_pr` | Disallowed via `conflicts_with = allow_dirty`; a normal lease on a dirty *preferred* workspace already routes through the dirty-recovery path, which `--resume_pr` does not engage. |
| Empty `@` | `cube pr push` | No-op success if bookmarks already at head; otherwise refuse (nothing to land). |
| Detached / non-descendant `@` | `cube pr push` | Refuse; instruct `--resume_pr` to rebuild on the head. |
| Push rejected (stale remote head) | `cube pr push` | Loud error; instruct `jj git fetch` + rebase. Never auto-force. |
| PR is MERGED/CLOSED | `--resume_pr`, `cube pr push` | `gh pr view … --json state` is consulted; warn (resume) / refuse (push) when the PR is not open — landing a commit on a merged PR's branch is almost always a mistake. |
| Attempt to push a `pr/*` bookmark | `cube pr ensure`, `cube pr push` | Refused by the push guard — the local-only invariant's enforcement point. |
| `pr/<n>` absent locally | `--resume_pr` | Fallback path: resolve head from GitHub, fetch, create the bookmark. Not an error. |

## Boss-integration sketch (follow-up — NOT implemented here)

This is how boss's revision flow (T1363) would *later* be rewired onto these primitives. It is recorded so the coordinator can file the follow-up work items; **no boss code changes are in scope for this project.**

Today (`revision-tasks.md` Q3) the engine pre-positions a revision worker with a raw-jj block in the spawn prelude, and the worker pushes with raw `jj bookmark set` + `jj git push`. The recommendation is to **reimplement that positioning and push on top of the cube primitives**, moving the jj choreography out of the boss engine directive and into cube — the owner of the contract:

- **Dispatch / positioning.** Where the coordinator leases a workspace for a `revision_implementation` execution (`lease_workspace_with_fallback`, which already reads `preferred_workspace_id` and the soft-prefer signal), pass **`--resume_pr <parent_pr_number> --prefer <chain-root's-last-workspace>`**. Cube lands `@` as a fresh empty commit on the parent PR's head. The raw `jj git fetch`/`jj edit`/`jj new` block is deleted from the spawn prelude. Boss keeps supplying the *policy* (which PR number, soft-prefer fallback); cube supplies the *positioning*.
- **Produce → push.** The worker's "advance the PR" tail becomes a single **`cube pr push`** call instead of `jj bookmark set <PARENT_BRANCH>` + `jj git push -b <PARENT_BRANCH>`. Cube handles bookmark advancement, github-remote disambiguation, fast-forward enforcement, and push verification.
- **Completion detection is unchanged.** The detector still confirms the parent PR head advanced and matches the chain root's `pr_url` (`revision-tasks.md` Q4). `cube pr push` does not open a new PR, so the no-new-PR invariant holds for free.
- **Recommendation on whether T1363's raw-jj positioning is reimplemented on these primitives: yes.** The raw-jj block was a stopgap precisely because these primitives did not exist. Once they do, the spawn prelude should call them so the local-mirror trap and push-verification protections apply to revisions too (they currently do not).

**Implied boss work items (filed after this design lands; tracked in boss, not cube):**
- *Boss-WI-A — Coordinator passes `--resume_pr` for revision dispatch.* Replace the raw-jj pre-position in the spawn prelude with `cube workspace lease --resume_pr <parent#>`, composing with the existing soft-prefer (`prefer_is_soft`) machinery. Medium.
- *Boss-WI-B — Revision worker directive calls `cube pr push`.* Replace the raw `jj bookmark set` + `jj git push` tail with `cube pr push`. Small.
- These two depend on cube T1–T3 (below) shipping first. They are *boss* work items and are `future / not a v1 blocker` for *this* (cube) project.

**Constraint check:** none of the cube primitives encode "revision = commit on parent PR." `pr/<n>`, resume-by-number, and advance-existing-PR are generic. The revision semantics stay in boss. ✔

## Risks / open questions

- **R1 — Local-only guarantee depends on cube discipline, not a jj feature.** jj has no first-class local-only bookmark. The guarantee is *namespace + cube never pushes `pr/*` + a push guard*. *Mitigation:* the guard turns any accidental `cube … --branch pr/<n>` into a loud error; cube already centralizes all pushes, so the surface area is small. *Open question:* should the guard also scan `jj git push` invocations cube does *not* originate (it cannot — those are the worker's own jj calls)? The honest boundary is "cube guarantees *its* pushes never carry `pr/*`"; a worker that hand-runs `jj git push -b pr/5 --all` is out of contract.
- **R2 — Stale `pr/<n>` after an external advance.** Another workspace advances PR N; this workspace's `pr/<n>` is now behind GitHub. *Mitigation:* every consumer (`--resume_pr`, `cube pr push`) fetches and reconciles against GitHub before trusting the local bookmark; `cube pr push`'s fast-forward-only rule turns a genuinely diverged state into a loud stale-push error rather than a silent clobber.
- **R3 — `cube pr push` PR-inference ambiguity.** Inferring the PR from `@`'s ancestry could be ambiguous if multiple `pr/<n>` bookmarks sit on the ancestry (e.g. stacked work). *Open question for reviewer:* is ancestry-inference acceptable as the default, with `--pr`/`--branch` as the explicit override, or should `cube pr push` *require* an explicit `--pr` in v1 to avoid surprises? (Leaning: infer-with-override; boss always knows the number and can pass it.)
- **R4 — Force-push policy.** v1 has no force path, so a worker that legitimately needs to rewrite a PR head (e.g. amend after a rebase) cannot use `cube pr push`. *Open question:* defer the `--force-with-lease` override entirely to v2, or include a guarded version now? (Leaning: defer; rewrites are rare and the safe default matters more.)
- **R5 — Merged/closed PR handling.** Should `--resume_pr` on a merged PR be a hard error or a warning-and-proceed? (Leaning: warn for resume — there are read-only inspection reasons; refuse for `cube pr push` — landing on a merged branch is almost always wrong.)
- **R6 — Backfill timing.** `pr/<n>` is only set the *next* time `cube pr ensure`/`cube pr push`/`--resume_pr` touches a PR. Existing in-flight PRs have no bookmark until then. *Mitigation:* the fallback path makes that a non-event (gh + fetch recovers the head); the bookmark is an optimization, not a correctness dependency.
- **R7 — Bookmark hygiene / GC.** The existing pool GC forgets merged `boss/exec_*` bookmarks on free workspaces (`gc_workspace_bookmarks`). *Open question:* should `pr/<n>` bookmarks be swept by the same GC once the PR is merged/closed, to avoid accumulation across reused workspaces? (Leaning: yes — extend the GC glob to also forget `pr/*` whose PR is closed; low priority, can be a follow-up.)

## Proposed implementation task breakdown

PR-sized tasks in dependency order. Effort hints: `trivial | small | medium | large`. "Parallel" notes mark tasks at the same dependency depth that can proceed concurrently.

### T1 — `pr/<n>` reserved namespace, helper, and push guard
**Scope:** Add the `pr/<n>` naming constant and a small helper module in cube (`is_pr_bookmark(name) -> bool`, `pr_bookmark_name(n) -> String`, matching `^pr/\d+$`). Add the push guard used by all push paths: refuse to `jj git push` any bookmark matching the reserved pattern. Unit tests for the matcher and guard. No behavior change to existing commands yet (the guard only fires on a `pr/*` push, which nothing does today). This is the shared substrate the other two commands build on.
**Effort:** small.
**Dependencies:** none.

### T2 — `cube pr ensure` sets the local `pr/<n>` bookmark
**Scope:** After the PR number is resolved in `ensure_pr` (existing or created), `jj bookmark set pr/<n> -r <branch>`. Tighten the `gh pr list` call to `--state open` and error on >1 open PR. Add `"pr_bookmark"` to the structured `RunResult` JSON (stdout contract unchanged — still the URL only). Wire in the T1 push guard before the existing `jj git push`. Tests: bookmark set on create path, on reuse/backfill path, multi-open-PR error, guard rejects a `pr/*` branch arg.
**Effort:** small.
**Dependencies:** T1.

### T3 — `cube pr push` subcommand
**Scope:** New `PrCommand::Push` with `--pr <n>` / `--branch <head-branch>` optional args. Implement PR/branch resolution (ancestry inference + explicit override), the snapshot/empty/detached guards, fast-forward-only advancement of both bookmarks, `jj git push -b <head-branch>` (no `--allow-new`) via the resolved github remote, push verification (reuse `verify_push_reached_github`), idempotent no-op, and the failure-mode handling in the table above. Reuse the T1 guard. Tests cover: happy-path advance, no-op idempotency, detached refusal, stale-push error surfacing, merged-PR refusal.
**Effort:** medium.
**Dependencies:** T1. *(May run in parallel with T4.)*

### T4 — `cube workspace lease --resume_pr <n>`
**Scope:** Add the `--resume_pr <n>` flag (clap, `conflicts_with = allow_dirty`, composes with `--prefer`). After a workspace is claimed, replace the normal `jj new <main>` reset with the resume positioning sequence: fetch → resolve PR head (local `pr/<n>` fast path; `gh pr view` + fetch fallback) → reconcile/set `pr/<n>` → re-establish the head-branch tracking bookmark → `jj new pr/<n>`. Surface `{pr_number, head_branch}` in the lease JSON; record the new `@` as `head_commit`. Tests: warm path (local bookmark present), cold path (bookmark absent → gh fallback), `--prefer` composition (preferred-free vs fallback), conflicts-with `--allow-dirty`, merged-PR warning.
**Effort:** medium.
**Dependencies:** T1. *(May run in parallel with T3 — both depend only on T1 and touch disjoint code paths: `app.rs` lease vs `app.rs` pr.)*

### T5 — Bookmark GC for closed-PR `pr/*` bookmarks
**Scope:** Extend `gc_workspace_bookmarks` (and the pool-wide GC) to also forget `pr/<n>` bookmarks whose PR is closed/merged, so they do not accumulate across reused free workspaces. Mirror the existing `boss/exec_*` sweep; resolve PR state via `gh` (or skip if offline). Tests: closed-PR `pr/*` forgotten, open-PR `pr/*` retained.
**Effort:** small.
**Dependencies:** T1. *(Independent of T3/T4; can run any time after T1. `future / not a v1 blocker` — pure hygiene; ship if convenient, otherwise defer.)*

### Boss-WI-A — Coordinator passes `--resume_pr` for revision dispatch *(boss repo)*
**Scope:** Replace the raw-jj pre-position in the revision spawn prelude with `cube workspace lease --resume_pr <parent_pr_number>`, composing with the existing `prefer_is_soft` soft-prefer machinery (`coordinator.rs` `lease_workspace_with_fallback`). Delete the `jj git fetch`/`jj edit`/`jj new` block from the directive.
**Effort:** medium.
**Dependencies:** T4. **`future / not a v1 blocker`** for the cube project — this is boss work filed separately after the design lands.

### Boss-WI-B — Revision worker directive calls `cube pr push` *(boss repo)*
**Scope:** Replace the worker's raw `jj bookmark set <PARENT_BRANCH>` + `jj git push -b <PARENT_BRANCH>` tail with a single `cube pr push`. Completion detection is unchanged.
**Effort:** small.
**Dependencies:** T3, Boss-WI-A. **`future / not a v1 blocker`** for the cube project.

### Dependency graph / parallelism summary

```
T1 (small) ──┬── T2 (small)
             ├── T3 (medium) ─┐
             ├── T4 (medium) ─┤        (T2, T3, T4, T5 all depth-2: parallelizable after T1)
             └── T5 (small) ──┘
                         │
   Boss-WI-A (medium, boss) ── depends on T4
   Boss-WI-B (small,  boss) ── depends on T3 + Boss-WI-A
```

- **Depth 1:** T1 (gates everything).
- **Depth 2 (parallel):** T2, T3, T4, T5 — each depends only on T1 and touches disjoint code, so all four may run concurrently.
- **Depth 3 (boss repo, `future`):** Boss-WI-A (needs T4), then Boss-WI-B (needs T3 + Boss-WI-A).

The cube v1 scope is **T1–T4** (T5 optional hygiene). The two Boss-WI items are the deferred follow-up the *Boss-integration sketch* describes — listed here, explicitly out of scope for this project, so the task graph is complete rather than silently truncated.
