# Robust change detection in checkleft

**Project:** P844 / `proj_18b3fff3a1244738_e8` — Robust change detection in checkleft
**Status:** Design (no implementation in this PR)
**Author:** Boss worker `exec_18b3fffb232a8060_ec`

## Goals

A core promise of checkleft is to accurately figure out _what changed_ and run checks only on that. Today that change-detection is split: the diff base is computed in each consumer repo's `.buildkite/steps/checks.sh` (picking shas, computing merge-bases, special-casing the GitHub merge queue in shell) and then passed into `checkleft run --base-ref=<sha>`. That shell layer has been fragile and repeatedly wrong — over a single day we shipped a string of point-fixes for scoping bugs across CI contexts.

This project makes checkleft **self-sufficient** about change detection. Concretely:

1. **Self-sufficiency invariant.** `checkleft run` with no scoping arguments determines the correct base and changed-file set on its own from the environment it finds itself in. The shell step degrades to `checkleft run` and Just Works. No shas, base refs, or merge-queue shell gymnastics are passed from the outside.
2. **Primary-branch agnostic.** Detect whether the integration branch is `main` or `master` (and respect CI-provided base-branch hints); never hardcode.
3. **Shallow-aware.** Detect shallow CI checkouts and deepen/unshallow enough history to compute the base, or fail with a clear, actionable error rather than silently mis-scoping.
4. **VCS-agnostic.** Work with either `jj` or `git`. In a jj repo, shell out to the colocated git repo where that is the simpler/correct primitive.
5. **One principled scenario→base matrix.** Enumerate every known GitHub Actions and Buildkite scenario, define the correct base for each, and make the PR-vs-merge-queue distinction (the two rules are _opposite_ and have caused most of the bugs) a single centralised decision in Rust — not a shell `if`.
6. **Extensively tested.** Pure unit tests for environment classification and base selection, plus functional/e2e tests that drive a _real_ git repo (init, branch, merge commit, shallow clone, jj-colocated) through each scenario and assert both the resolved base and the scoped file set.

The end state: the per-repo `checks.sh` scoping logic — including the most recent T843/PR #948 3-dot merge-base fix — is retired, and that whole class of shell-side patching becomes unnecessary.

### Bug history this design must prevent recurring

These are the regressions that motivate the work; they double as required test fixtures.

- **Merge-queue fork-point bug (T774 / PR #910).** Merge-queue builds used `git merge-base HEAD^1 HEAD^2`, which returns the _fork point_ where the PR branched off main — many commits behind the queue tip. That swept in every unrelated change merged to main since the PR forked (e.g. `github_oauth.rs`), inflating the diff with files the PR never touched. Correct base: `HEAD^1`.
- **Regular-PR 2-dot bug (T843 / PR #948).** Regular PR builds diffed against `origin/main` directly (2-dot), flagging files that changed _only on main_ after the branch forked. Correct base: `merge-base(origin/main, HEAD)` (3-dot equivalent).
- **Files-changed-only-on-main false positives (build 1053 / PR #945).** Same failure mode as T843 from a different angle: main's divergence being attributed to the PR.
- **"Always scope to changes" churn (PR b12d4ede).** `--all` was being run automatically in CI and then walked back; scoping must default to changed-files-only, with `--all` reserved as a manual escape hatch.

## Non-goals

- **Not a general CI-provider abstraction.** We support the providers we actually run on (Buildkite, GitHub Actions) plus local developer machines. We do not build a plugin system for arbitrary CI systems; adding one later is a small, localised change to the environment classifier.
- **No change to _what_ checks run or how findings are produced.** This project only changes how the `ChangeSet` (base revision + changed files + per-file diffs) is resolved. The check registry, config resolution, and runner are untouched.
- **No new diff/line-delta parsing.** `parse_git_name_status`, `parse_jj_diff_summary`, and `patch_line_deltas` already work and are reused as-is.
- **Not removing `--base-ref` / `--all`.** They remain as explicit operator overrides (escape hatches). The goal is that _nobody needs them in normal CI_, not that they cease to exist.
- **No mutation of remote state beyond fetch.** Deepening history (`git fetch --deepen` / `--unshallow`) is the only network side effect. We never push, rewrite, or create refs.
- **Not solving submodule or monorepo-subtree scoping.** checkleft already scopes by path globs in config; nothing here changes that.

## Background: how scoping flows today

`checkleft run` resolves a `ChangeSet` through two coupled paths in `tools/checkleft/src/vcs.rs`:

- **Changed-file set:** `resolve_changeset()` (main.rs) calls one of `Vcs::current_changeset()` (uncommitted working-tree diff), `Vcs::changeset_since(base_ref)` (committed diff vs a base), or `Vcs::all_files_changeset()` (`--all`).
- **Base tree reads:** `Vcs::base_revision(all, base_ref)` resolves the revision used by `LocalSourceTree::read_base_file()` so diff-aware checks can read the _old_ version of a file (`git show <rev>:<path>` / `jj file show -r <rev>`).

For Git, both paths funnel through `resolve_git_merge_base(root, base_ref) = git merge-base <base_ref> HEAD`, then diff `<merge_base>..HEAD` (a 2-dot diff against the merge-base, i.e. the 3-dot result relative to `base_ref`). So checkleft **already** does the right 3-dot computation _given the right base_ref_ — the fragility is entirely in `checks.sh` choosing what `base_ref` to pass:

| CI scenario  | `checks.sh` passes as `--base-ref`               | checkleft then computes                                       |
| ------------ | ------------------------------------------------ | ------------------------------------------------------------- |
| Regular PR   | `merge-base(origin/<base>, HEAD)` (pre-resolved) | `merge-base(that, HEAD)` = same (idempotent)                  |
| Merge queue  | `HEAD^1`                                         | `merge-base(HEAD^1, HEAD)` = `HEAD^1` (HEAD^1 is an ancestor) |
| Push to main | `merge-base(HEAD, origin/main)`                  | same                                                          |

The merge-queue rule works _only_ because `checks.sh` passes `HEAD^1` rather than `origin/main`; had it passed `origin/main`, checkleft's `merge-base` would silently produce the fork point — the T774 bug. **The correct base is therefore scenario-dependent, and the scenario is only knowable from environment variables that checkleft does not currently read.** That is the gap this design closes: move the scenario classification and base selection _into_ checkleft.

## Alternatives considered

### Alternative A — Keep base selection in shell, just harden `checks.sh`

Continue computing the base in `checks.sh` (or a shared, vendored shell helper) and keep passing `--base-ref`. Fix the bugs in shell and add shell-level tests.

**Rejected because:**

- It does not satisfy the self-sufficiency invariant — every consumer repo still re-derives and maintains its own script, and every new CI context is a new shell patch.
- Shell is the proven-fragile layer; the bug history is _entirely_ in shell. `bash` has no real test harness for "given these env vars and this git topology, what base do we pick", so regressions ship.
- The PR-vs-merge-queue distinction stays a shell `if`, which is exactly the construct that has been wrong repeatedly.

### Alternative B — Always diff the working tree / always `--all`

Sidestep base computation: either run every check over all tracked files, or diff only the uncommitted working tree.

**Rejected because:**

- `--all` defeats checkleft's core value (scope to changes) and re-introduces the "always scope to changes" churn (PR b12d4ede) in reverse — it floods PRs with findings about pre-existing violations the author didn't touch.
- Working-tree-only diff is empty in CI (the checkout is a clean commit), so CI would check nothing. It also can't express "what this PR contributes" on a merge commit.

### Alternative C — Trust CI-provided base shas directly (no merge-base in checkleft)

Read the base sha CI already knows (`github.event.pull_request.base.sha`, `github.event.merge_group.base_sha`, Buildkite's base-branch) and diff `base..HEAD` 2-dot with no merge-base.

**Rejected as the _sole_ mechanism because:**

- The PR base sha is the _current_ tip of the base branch at build time, not the fork point. A 2-dot diff against it is exactly the T843 bug. We still need `merge-base` for the PR case.
- However, the _signals_ CI provides (event name, base ref, merge-group base sha) are the most reliable way to _classify the scenario_. So we **adopt the signals, reject the naive 2-dot diff.** This informs the chosen approach: use env vars to classify, then apply the scenario-correct base rule (which for PRs is still a merge-base).

### Alternative D (chosen) — A self-contained change-detection module in checkleft

Introduce a `change_detection` module that (1) classifies the environment into a `Scenario`, (2) resolves the default branch, (3) ensures enough history (deepen if shallow), (4) selects the base per a single matrix, and (5) hands the resolved base to the _existing_ `changeset_since` / `base_revision` plumbing. `checks.sh` shrinks to `checkleft run`. Chosen because it is the only option that satisfies self-sufficiency, centralises the opposite PR/merge-queue rules in testable Rust, and reuses the diff machinery that already works.

## Chosen approach

### Module shape

A new module `tools/checkleft/src/change_detection/` with these units (names indicative):

```
change_detection/
  mod.rs          // public entry: resolve_change_plan(env, vcs, overrides) -> ChangePlan
  environment.rs  // CiEnvironment: a struct read from env vars (injectable for tests)
  scenario.rs     // Scenario enum + classify(env) -> Scenario  (PURE, heavily unit-tested)
  base.rs         // select_base(scenario, vcs, default_branch) -> BaseSelection
  default_branch.rs // resolve_default_branch(env, vcs) -> "main" | "master" | hint
  shallow.rs      // ensure_history(vcs, needed_ref) -> deepen/unshallow or clear error
```

The **public entry point** is a single pure-ish function:

```rust
pub struct ChangeOverrides {
    pub all: bool,                 // --all: ignore scope, check every tracked file
    pub base_ref: Option<String>,  // --base-ref: explicit operator override
    pub default_branch: Option<String>, // --default-branch: rarely needed escape hatch
}

pub enum ChangePlan {
    All,                                 // check every tracked file
    Scoped { base: BaseRevision, scenario: Scenario },
    Empty { reason: EmptyReason },       // first commit / no merge-base / detached, see fallbacks
}

pub fn resolve_change_plan(
    env: &CiEnvironment,
    vcs: &Vcs,
    overrides: &ChangeOverrides,
) -> Result<ChangePlan>;
```

`main.rs::resolve_changeset` then becomes: build a `CiEnvironment` from `std::env`, call `resolve_change_plan`, and translate the `ChangePlan` into the existing `Vcs` calls (`all_files_changeset` / `changeset_since` / a no-op empty set). `base_revision()` is derived from the same `ChangePlan` so the changed-file set and the base-tree reads can never disagree (today they re-derive independently — a latent footgun).

### `CiEnvironment` — the only place env vars are read

A plain struct populated from environment variables, passed by value so every downstream function is pure and table-testable. Captured variables:

- **Buildkite:** `BUILDKITE`, `BUILDKITE_PULL_REQUEST` (`"false"` or a number), `BUILDKITE_PULL_REQUEST_BASE_BRANCH`, `BUILDKITE_BRANCH`, `BUILDKITE_COMMIT`, `BUILDKITE_PIPELINE_DEFAULT_BRANCH`.
- **GitHub Actions:** `GITHUB_ACTIONS`, `GITHUB_EVENT_NAME` (`pull_request` / `push` / `merge_group`), `GITHUB_BASE_REF`, `GITHUB_HEAD_REF`, `GITHUB_REF`, `GITHUB_SHA`, `GITHUB_EVENT_PATH` (path to the event JSON, read lazily only when we need `merge_group.base_sha`/`head_sha` or `repository.default_branch`).
- **Generic:** `CI`.

No other code in checkleft reads these; tests construct `CiEnvironment` literals directly.

### Scenario classification (the centralised decision)

`classify(env) -> Scenario` is a pure function and the single source of truth for the PR-vs-merge-queue distinction. Precedence is explicit and ordered so ambiguous combinations are deterministic:

```
1. Explicit override (overrides.base_ref / overrides.all)  -> handled before classify
2. GitHub merge_group  (GITHUB_EVENT_NAME == "merge_group")            -> MergeQueue
3. Buildkite merge queue (BUILDKITE_BRANCH starts "gh-readonly-queue/")-> MergeQueue
4. GitHub pull_request (GITHUB_EVENT_NAME == "pull_request")           -> PullRequest
5. Buildkite PR       (BUILDKITE_PULL_REQUEST != "false")              -> PullRequest
6. Push to default    (push event AND branch == default branch)       -> PushToDefault
7. Push to other ref  (push event, non-default branch)                -> PushToBranch
8. No CI signal                                                        -> Local
```

```rust
pub enum Scenario {
    PullRequest { base_branch: String },  // base_branch from CI hint or default
    MergeQueue,                            // HEAD is a GitHub-created merge commit
    PushToDefault,
    PushToBranch { branch: String },
    Local,
}
```

### Scenario → base matrix

This is the heart of the design. Every row is a unit test (env literal → expected `Scenario`) and an e2e test (real git topology → expected base + scoped files).

| #   | Scenario                             | Detection signals                                                        | Base revision                                                                                                       | Diff semantics                       | Notes / bug guarded                                                                                                                                |
| --- | ------------------------------------ | ------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------- | ------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **Regular PR**                       | GHA `pull_request`, or BK `BUILDKITE_PULL_REQUEST != false`              | `merge-base(<base-branch-ref>, HEAD)`                                                                               | 3-dot equiv (diff `mergebase..HEAD`) | Base branch = `GITHUB_BASE_REF` / `BUILDKITE_PULL_REQUEST_BASE_BRANCH`, else default branch. **Must NOT 2-dot against `origin/base`** (T843/#948). |
| 2   | **GitHub merge queue**               | GHA `merge_group`                                                        | `merge_group.base_sha` from event payload — equivalently `HEAD^1`                                                   | 2-dot `base..HEAD`                   | base_sha == the main tip being merged onto. **Must NOT `merge-base(HEAD^1,HEAD^2)`** (T774/#910).                                                  |
| 3   | **Buildkite merge queue**            | `BUILDKITE_BRANCH` = `gh-readonly-queue/<branch>/...`                    | `HEAD^1` (require HEAD be a merge commit; fall back to rule 1 against the queue's target branch if not)             | 2-dot `HEAD^1..HEAD`                 | Target branch parsed from the `gh-readonly-queue/<target>/` segment.                                                                               |
| 4   | **Push to default** (main/master)    | push event, branch == default                                            | `HEAD^1` if HEAD is a normal commit; if HEAD is a merge commit, `HEAD^1`; range `before..after` when CI provides it | range / `parent..HEAD`               | Scope to this push only, not full history. Prefer CI `before` sha when present and reachable.                                                      |
| 5   | **Push to non-default branch**       | push event, branch != default                                            | `merge-base(default-branch, HEAD)`                                                                                  | 3-dot equiv                          | Treated like a pre-merge branch; same rule as PR without a PR number.                                                                              |
| 6   | **Local / pre-push**                 | no CI env                                                                | `merge-base(default-branch, working tree)`, **including uncommitted + staged changes**                              | 3-dot equiv + working tree           | Developer machine. jj: `@-` baseline plus working-copy changes; git: `merge-base` + `git diff HEAD`.                                               |
| 7   | **First commit / no merge-base**     | merge-base computation yields nothing (root commit, unrelated histories) | —                                                                                                                   | —                                    | `ChangePlan::Empty { NoMergeBase }` → check nothing (or `--all` if operator opts in). Logged clearly, exit 0.                                      |
| 8   | **Detached HEAD**                    | `HEAD` not on a branch and no CI hint                                    | best-effort: `HEAD^1` if it exists, else `Empty`                                                                    | parent..HEAD                         | Common in some CI checkouts; never error hard.                                                                                                     |
| 9   | **Forced shallow, base unreachable** | shallow repo, base ref not in local history                              | deepen/unshallow, then recompute; if still unreachable → clear error                                                | —                                    | See "Shallow handling".                                                                                                                            |

**The opposite rules (rows 1 vs 2/3) live in exactly one place** — `base.rs::select_base` matching on `Scenario`. There is no shell `if`, and the two rules are adjacent and commented so the asymmetry is impossible to miss.

### Default-branch resolution

`resolve_default_branch(env, vcs)` tries, in order, and returns the first that resolves to an existing ref:

1. Explicit `--default-branch` override.
2. CI hint: `BUILDKITE_PIPELINE_DEFAULT_BRANCH`; GHA `repository.default_branch` from the event payload; the `<target>` segment of a `gh-readonly-queue/<target>/...` ref.
3. `git symbolic-ref refs/remotes/origin/HEAD` (the remote's default branch) → strip to short name.
4. Probe `origin/main` then `origin/master` (then local `main`/`master`) and pick whichever exists.
5. Fallback: `main`, with a warning.

This satisfies requirement 1 (main _or_ master, detected not hardcoded) and is independently unit-testable by stubbing the ref-existence probe.

### Shallow handling

CI checkouts are frequently shallow (`git clone --depth=1`), so the base commit / merge-base may not be present locally. `shallow.rs::ensure_history(vcs, needed)`:

1. `git rev-parse --is-shallow-repository`. If false, done.
2. If shallow, attempt to make the base reachable with bounded work:
   - For PR/branch scenarios, `git fetch --deepen=<N>` in increasing steps (e.g. 50 → 250 → 1000) and re-test `git merge-base` reachability after each, capping total work; fall back to `git fetch --unshallow origin` as the last resort.
   - For merge-queue/push scenarios we only need `HEAD^1`, which a depth-2 fetch guarantees: `git fetch --deepen=1` is enough.
3. If after unshallow the base is _still_ unreachable (e.g. base branch was never fetched), emit a precise error: which ref, what we tried, and the one-line remedy (fetch the base branch / increase clone depth). Never silently fall back to diffing against the tip — that is the failure mode that produced the original bugs.

For **jj** colocated repos the underlying git repo is what is shallow; we operate on it via the colocated `.git` (jj does not itself fetch history for merge-base). Detection: if `Vcs` is `Jujutsu` but a colocated git dir exists, shallow operations shell out to `git` in the repo root (already the pattern `source_tree.rs` uses for base reads).

### jj vs git handling

- **VCS detection** is unchanged (`Vcs::detect` already prefers jj, falls back to git).
- **Changed-file set** reuses `changeset_since` / `current_changeset`, which already branch on `VcsKind`.
- **Base selection** is computed in git terms (merge-base, `HEAD^1`, shas) because CI topology _is_ git. In a jj repo we translate: a resolved git sha is a valid jj revision (jj accepts git commit ids), so `BaseRevision::Jujutsu(sha)` works for `jj diff --from <sha>`. Where we need `HEAD^1` semantics we resolve the concrete sha via the colocated git repo first, then hand the sha to jj. This keeps one base-selection code path rather than two parallel jj/git matrices.
- Requirement 5 ("in a jj repo it may shell out to git") is satisfied: scenario classification and base _resolution_ use git; the _diff_ uses whichever VCS is native.

### Self-sufficiency API surface

What `checkleft run` resolves on its own vs. optional overrides:

| Input                     | Source                                      | Default behaviour                                                                                                 |
| ------------------------- | ------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| Scenario                  | env (`CiEnvironment`)                       | auto-classified; Local when no CI signal                                                                          |
| Default branch            | CI hint → `origin/HEAD` → probe main/master | auto                                                                                                              |
| Base revision             | scenario→base matrix                        | auto                                                                                                              |
| Shallow depth             | auto-deepen as needed                       | auto                                                                                                              |
| `--all`                   | operator flag                               | off (scope to changes)                                                                                            |
| `--base-ref=<ref>`        | operator flag                               | unset; when set, **bypasses classification** and uses `merge-base(ref, HEAD)` as today (back-compat escape hatch) |
| `--default-branch=<name>` | operator flag                               | unset; overrides resolution step 1                                                                                |

The intended steady state in CI is literally `checkleft run` (plus existing `--external-checks-*` config). `--base-ref` is retained only for humans debugging or for an exotic context we haven't enumerated.

### Migration: retiring the `checks.sh` scoping logic

The migration is staged so we never have a window where CI mis-scopes:

1. **Land checkleft change** (behind the auto-classification path). Keep `--base-ref` honoured exactly as today, so the _current_ `checks.sh` keeps working unchanged. No behaviour change yet.
2. **Verify in CI** that `checkleft run` (no args) classifies and scopes identically to the current `--base-ref` path across all live scenarios — run both in parallel for a short bake (log the resolved base from each and assert equality in a temporary CI step).
3. **Shrink `checks.sh`** to drop the entire `if/elif/else` base-computation block and the `--unshallow` block; the step becomes the repobin install + `bin/checkleft run`. This retires the T774/T843/#948/#910/#945 shell logic in one commit.
4. **Document** in checkleft's README/usage that change detection is automatic and `checks.sh`-style sha plumbing is no longer needed; point other consumer repos at the same one-liner.

Because the consumer (`mono`) is the same repo as checkleft, steps 1 and 3 are separate PRs in the same tree; step 3 explicitly references and removes PR #948's merge-base block.

### Observability

`checkleft run` logs (at info, already wired via `tracing`) the classified scenario, the resolved default branch, whether a deepen/unshallow happened, and the final base sha + changed-file count. This replaces the `echo "[checks] ..."` breadcrumbs in `checks.sh` so CI logs remain debuggable after the shell shrinks.

## Risks / open questions

1. **GitHub event payload parsing.** Rows 2 and the default-branch resolver read `GITHUB_EVENT_PATH` JSON (`merge_group.base_sha`, `repository.default_branch`). This couples us to GitHub's event schema. Mitigation: treat payload fields as optional, fall back to `HEAD^1` / ref-probing when absent; unit-test against captured real payloads. _Open: do we want to vendor a small fixture set of real event JSONs?_
2. **Auto-deepen cost.** `git fetch --unshallow` on a large repo is slow. The bounded `--deepen` ladder mitigates this, but the step sizes (50/250/1000) are guesses. _Open: should depth be configurable via env, and what is the right cap before we hard-error rather than unshallow fully?_
3. **jj + shallow interaction.** jj's colocated git may behave differently under `--deepen` than a plain git checkout (jj maintains its own op log). _Open: confirm `jj` tolerates the git repo being deepened underneath it without a `jj git import` step — likely needs one._
4. **Local working-tree scope semantics (row 6).** Should local `checkleft run` include staged-but-uncommitted and untracked files by default, matching `current_changeset()` today, or only committed work since the fork? Proposed: include uncommitted + staged (matches developer intuition "check what I'm about to push"), exclude untracked unless added. _Needs operator confirmation._
5. **Detached HEAD in CI (row 8).** Some providers check out a detached HEAD even for PRs. We rely on env signals to classify before falling back to HEAD topology; if a provider gives neither a usable env signal nor a branch, we degrade to `HEAD^1`. _Open: is `Empty` (check nothing) or `HEAD^1` (check last commit) the safer default when truly ambiguous?_
6. **Back-compat of `--base-ref`.** Keeping the old idempotent `merge-base(base_ref, HEAD)` behaviour means a caller passing `HEAD^1` still works. We must keep that exact semantics during the bake (step 2) so the parallel-run equality check is meaningful.
7. **Push-to-default range (row 4).** Buildkite/GHA both _can_ provide a `before` sha for pushes, but it is `000…000` for branch-creation and can be unreachable after force-push. Proposed: use `before..after` only when `before` is non-zero and reachable, else `HEAD^1`. _Confirm this is acceptable for main pushes, which are normally fast-forward merges._

## Proposed implementation task breakdown

Tasks are PR-sized and listed in dependency order. "Depth" indicates which tasks may run in parallel (same depth, no edge between them).

### Depth 0 (no dependencies — may run in parallel)

**T1. `CiEnvironment` capture struct**
Scope: Introduce `change_detection/environment.rs` with a `CiEnvironment` struct populated from `std::env` (Buildkite + GHA + generic vars listed in the design), plus lazy `GITHUB_EVENT_PATH` JSON access behind a small typed reader. Pure constructor `from_env()` and a test constructor taking explicit values. No classification logic yet.
Effort: small.
Dependencies: none.

**T2. `Scenario` enum + pure `classify()`**
Scope: Add `change_detection/scenario.rs` with the `Scenario` enum and `classify(&CiEnvironment) -> Scenario` implementing the ordered precedence (merge_group → BK queue → PR → push-default → push-branch → local). Exhaustive table-driven unit tests, one case per matrix row plus ambiguity/precedence cases. No git calls.
Effort: medium.
Dependencies: T1.

**T3. Default-branch resolver**
Scope: Add `change_detection/default_branch.rs` with `resolve_default_branch(env, vcs)` implementing the CI-hint → `origin/HEAD` → probe-main/master → fallback ladder. Abstract ref-existence behind a trait/closure so the ladder is unit-testable without a real repo; one integration test against a real repo for the `origin/HEAD` path.
Effort: medium.
Dependencies: T1 (for env hints); can develop in parallel with T2.

### Depth 1

**T4. Base selection matrix (`select_base`)**
Scope: Add `change_detection/base.rs` mapping `Scenario` + default branch → `BaseSelection` (the resolved base revision and diff semantics), implementing every row of the scenario→base matrix. This is where the opposite PR (merge-base) vs merge-queue (`HEAD^1`) rules live, adjacent and commented. Resolve `merge_group.base_sha`/`HEAD^1`/merge-base to concrete shas via git. Heavy unit tests with a stubbed git-command layer.
Effort: large.
Dependencies: T2, T3.

**T5. Shallow detection + bounded deepen/unshallow**
Scope: Add `change_detection/shallow.rs` with `ensure_history(vcs, needed_ref)` — `is-shallow-repository` check, the bounded `--deepen` ladder with reachability re-test, `--unshallow` last resort, and the precise hard-error when the base remains unreachable. jj-colocated handling included.
Effort: medium.
Dependencies: T1 (uses VCS handle); independent of T4, so T4 and T5 may run in parallel.

### Depth 2

**T6. `resolve_change_plan` orchestration + wire into `main.rs`**
Scope: Add `change_detection/mod.rs` with `resolve_change_plan(env, vcs, overrides) -> ChangePlan` that composes classify → resolve default branch → ensure history → select base, handling the `Empty` fallbacks (rows 7/8/9). Replace `main.rs::resolve_changeset` and the separate `base_revision` derivation so the changed-file set and base-tree reads come from one `ChangePlan`. Preserve `--base-ref`/`--all` as overrides with exact current semantics. Add `--default-branch` flag.
Effort: medium.
Dependencies: T4, T5.

**T7. e2e/functional test harness driving a real git repo**
Scope: Add an integration test module that, per matrix row, constructs a real git repo (init main/master, fork a branch, add base-drift commits, build a synthetic merge commit for the queue cases, make a shallow clone, and a jj-colocated variant) and asserts both the resolved base sha and the scoped file set. Encode the T774, T843/#948, and #945 fixtures explicitly as named tests. Reuses the existing `git_changeset_since_excludes_base_branch_drift` test as a template.
Effort: large.
Dependencies: T6 (needs the orchestrator) — though the harness scaffolding (repo-builder helpers) can begin in parallel at depth 1 and only the assertions wait on T6.

### Depth 3

**T8. Shrink `checks.sh` and document the migration**
Scope: After CI bake confirms parity, remove the `if/elif/else` base-computation and `--unshallow` blocks from `.buildkite/steps/checks.sh`, reducing it to repobin-install + `bin/checkleft run`. Explicitly retires PR #948's merge-base logic. Update checkleft usage docs to state change detection is automatic. (Per repo convention, the `checks.sh` change is code, not docs — normal PR; the doc update may go to main directly.)
Effort: small.
Dependencies: T6, T7.

**T9. Parallel-run parity bake step (temporary)**
Scope: Add a short-lived CI step that runs both the old `--base-ref` path and the new auto-classification path, logs both resolved bases, and asserts equality across live scenarios. Removed once confidence is established (folded into T8's removal). This is the safety net for the migration.
Effort: small.
Dependencies: T6. May run in parallel with T7.

### Deferred / out of scope (`future / not a v1 blocker`)

- **F1. Pluggable CI-provider registry.** Generalising `CiEnvironment` into a provider plugin system for CI systems beyond Buildkite/GHA. `future / not a v1 blocker` — add only when a third provider appears.
- **F2. Configurable deepen depth / cap via env or config.** Risk #2. Ship with the fixed ladder first; make it configurable only if a repo hits the cap. `future / not a v1 blocker`.
- **F3. Vendored corpus of real GitHub event payloads** for regression-testing payload parsing (risk #1). Nice-to-have hardening. `future / not a v1 blocker`.
- **F4. Untracked-file inclusion policy for local runs** (risk #4). Pending operator decision; default excludes untracked. `future / not a v1 blocker`.
- **F5. Submodule / subtree-aware scoping.** Explicitly out of scope (non-goal). `future / not a v1 blocker`.
