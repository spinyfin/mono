# Robust change detection in checkleft

- **Project:** P844 / `proj_18b3fff3a1244738_e8` — Robust change detection in checkleft
- **Status:** Implemented — shipped to `main` across PRs #1085, #1090, #1094, #1099, #1104, #1107, #1121 (2026-05-31 → 2026-06-01)
- **Author:** Boss worker `exec_18b3fffb232a8060_ec`; revised post-implementation to reflect as-built reality by `exec_18c40f0bacc19a28_622`

## Goals

A core promise of checkleft is to accurately figure out _what changed_ and run checks only on that. Before this project, that change-detection was split: the diff base was computed in each consumer repo's `.buildkite/steps/checks.sh` (picking shas, computing merge-bases, special-casing the GitHub merge queue in shell) and then passed into `checkleft run --base-ref=<sha>`. That shell layer was fragile and repeatedly wrong — over a single day we shipped a string of point-fixes for scoping bugs across CI contexts.

This project made checkleft **self-sufficient** about change detection. Concretely:

1. **Self-sufficiency invariant.** `checkleft run` with no scoping arguments determines the correct base and changed-file set on its own from the environment it finds itself in. The shell step degrades to `checkleft run` and Just Works. No shas, base refs, or merge-queue shell gymnastics are passed from the outside.
2. **Primary-branch agnostic.** Detect whether the integration branch is `main` or `master` (and respect CI-provided base-branch hints); never hardcode.
3. **Shallow-aware.** Detect shallow CI checkouts and deepen/unshallow enough history to compute the base, or fail with a clear, actionable error rather than silently mis-scoping.
4. **VCS-agnostic.** Work with either `jj` or `git`. In a jj repo, shell out to the colocated git repo where that is the simpler/correct primitive.
5. **One principled scenario→base matrix.** Enumerate every known GitHub Actions and Buildkite scenario, define the correct base for each, and make the PR-vs-merge-queue distinction (the two rules are _opposite_ and have caused most of the bugs) a single centralised decision in Rust — not a shell `if`.
6. **Extensively tested.** Pure unit tests for environment classification and base selection, plus functional/e2e tests that drive a _real_ git repo (init, branch, merge commit, shallow clone, jj-colocated) through each scenario and assert both the resolved base and the scoped file set.

The end state shipped in PR #1121: the per-repo `checks.sh` scoping logic — including the T843/PR #948 3-dot merge-base fix and the T774/PR #910 merge-queue handling — is retired, and `checks.sh` is now just repobin install + `bin/checkleft run`.

### Bug history this design must prevent recurring

These are the regressions that motivated the work; each is encoded as a named regression test in the e2e suite (PR #1107).

- **Merge-queue fork-point bug (T774 / PR #910).** Merge-queue builds used `git merge-base HEAD^1 HEAD^2`, which returns the _fork point_ where the PR branched off main — many commits behind the queue tip. That swept in every unrelated change merged to main since the PR forked (e.g. `github_oauth.rs`), inflating the diff with files the PR never touched. Correct base: `HEAD^1`.
- **Regular-PR 2-dot bug (T843 / PR #948).** Regular PR builds diffed against `origin/main` directly (2-dot), flagging files that changed _only on main_ after the branch forked. Correct base: `merge-base(origin/main, HEAD)` (3-dot equivalent).
- **Files-changed-only-on-main false positives (build 1053 / PR #945).** Same failure mode as T843 from a different angle: main's divergence being attributed to the PR.
- **"Always scope to changes" churn (PR b12d4ede).** `--all` was being run automatically in CI and then walked back; scoping must default to changed-files-only, with `--all` reserved as a manual escape hatch.
- **Buildkite merge-queue `HEAD^2` sentinel bug (T1016 / PR #1104).** Found _during this project's own bake period_, validating the staged migration. The first implementation of the Buildkite-queue rule verified HEAD was a merge commit by probing `HEAD^2` and fell back to the PR rule when the probe failed. In a shallow Buildkite checkout the second parent is often not fetched even when HEAD is a genuine merge commit, so the sentinel failed and the fallback computed `merge-base(origin/main)` — 321 changed files instead of the correct 6. Fix: use `HEAD^1` unconditionally for the Buildkite queue, exactly mirroring what legacy `checks.sh` did.

## Non-goals

- **Not a general CI-provider abstraction.** We support the providers we actually run on (Buildkite, GitHub Actions) plus local developer machines. We do not build a plugin system for arbitrary CI systems; adding one later is a small, localised change to the environment classifier.
- **No change to _what_ checks run or how findings are produced.** This project only changes how the `ChangeSet` (base revision + changed files + per-file diffs) is resolved. The check registry, config resolution, and runner are untouched.
- **No new diff/line-delta parsing.** `parse_git_name_status`, `parse_jj_diff_summary`, and `patch_line_deltas` already work and are reused as-is.
- **Not removing `--base-ref` / `--all`.** They remain as explicit operator overrides (escape hatches). The goal is that _nobody needs them in normal CI_, not that they cease to exist.
- **No mutation of remote state beyond fetch.** Deepening history (`git fetch --deepen` / `--unshallow`) is the only network side effect. We never push, rewrite, or create refs.
- **Not solving submodule or monorepo-subtree scoping.** checkleft already scopes by path globs in config; nothing here changes that.

## Background: how scoping flowed before this project

`checkleft run` resolved a `ChangeSet` through two coupled paths in `tools/checkleft/src/vcs.rs`:

- **Changed-file set:** `resolve_changeset()` (main.rs) called one of `Vcs::current_changeset()` (uncommitted working-tree diff), `Vcs::changeset_since(base_ref)` (committed diff vs a base), or `Vcs::all_files_changeset()` (`--all`).
- **Base tree reads:** `Vcs::base_revision(all, base_ref)` resolved the revision used by `LocalSourceTree::read_base_file()` so diff-aware checks could read the _old_ version of a file (`git show <rev>:<path>` / `jj file show -r <rev>`).

For Git, both paths funnelled through `resolve_git_merge_base(root, base_ref) = git merge-base <base_ref> HEAD`, then diffed `<merge_base>..HEAD` (a 2-dot diff against the merge-base, i.e. the 3-dot result relative to `base_ref`). So checkleft **already** did the right 3-dot computation _given the right base_ref_ — the fragility was entirely in `checks.sh` choosing what `base_ref` to pass:

| CI scenario  | `checks.sh` passed as `--base-ref`               | checkleft then computed                                       |
| ------------ | ------------------------------------------------ | ------------------------------------------------------------- |
| Regular PR   | `merge-base(origin/<base>, HEAD)` (pre-resolved) | `merge-base(that, HEAD)` = same (idempotent)                  |
| Merge queue  | `HEAD^1`                                         | `merge-base(HEAD^1, HEAD)` = `HEAD^1` (HEAD^1 is an ancestor) |
| Push to main | `merge-base(HEAD, origin/main)`                  | same                                                          |

The merge-queue rule worked _only_ because `checks.sh` passed `HEAD^1` rather than `origin/main`; had it passed `origin/main`, checkleft's `merge-base` would silently produce the fork point — the T774 bug. **The correct base is therefore scenario-dependent, and the scenario is only knowable from environment variables that checkleft did not read.** That was the gap this design closed: move the scenario classification and base selection _into_ checkleft.

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

## Approach (as built)

### Module shape

The module `tools/checkleft/src/change_detection/` shipped with exactly the planned units:

```
change_detection/
  mod.rs          // public entry: resolve_change_plan(env, vcs, overrides) -> ChangePlan,
                  // base_revision_from_plan(vcs, plan), GitRefProber (production RefProber)
  environment.rs  // CiEnvironment: the only place CI env vars are read (injectable for tests)
  scenario.rs     // Scenario enum + classify(env, default_branch) -> Scenario  (PURE, table-tested)
  base.rs         // select_base(scenario, env, prober, default_branch) -> BaseSelection
                  // + HeadProber trait (injectable) and GitHeadProber (production)
  default_branch.rs // resolve_default_branch(env, prober, override) + RefProber trait
  shallow.rs      // ensure_history(root, kind, needed_ref, scenario) -> deepen/unshallow or clear error
```

One structural addition relative to the original sketch: every git interaction below the orchestrator goes through a small injected trait (`RefProber` for ref existence/`symbolic-ref`, `HeadProber` for `rev-parse`/`merge-base`), so the classification ladder, the default-branch ladder, and every matrix row are unit-testable with stubs and no real repository.

The **public entry point**:

```rust
pub struct ChangeOverrides {
    pub all: bool,                 // --all: ignore scope, check every tracked file
    pub base_ref: Option<String>,  // --base-ref: explicit operator override
    pub default_branch: Option<String>, // --default-branch: rarely needed escape hatch
}

pub enum ChangePlan {
    All,                                   // check every tracked file
    Scoped { base_sha: String, scenario: Scenario },
    Empty { reason: EmptyReason },         // first commit / no merge-base / detached, see fallbacks
}

pub fn resolve_change_plan(
    env: &CiEnvironment,
    vcs: &Vcs,
    overrides: &ChangeOverrides,
) -> Result<ChangePlan>;

pub fn base_revision_from_plan(vcs: &Vcs, plan: &ChangePlan) -> Option<BaseRevision>;
```

This diverges slightly from the original sketch: `Scoped` carries the fully resolved **concrete sha** (not a `BaseRevision`), and a companion `base_revision_from_plan` derives the source-tree read revision from that same sha. The property the design demanded — the changed-file set and the base-tree reads can never disagree, because both derive from one `ChangePlan` — holds; the old dual-derivation (`resolve_changeset` + an independent `base_revision`) was removed from `main.rs` in PR #1099.

Resolution order in `resolve_change_plan`: overrides short-circuit first (`--all` before any git call; `--base-ref` bypasses classification entirely with the old idempotent `merge-base(ref, HEAD)` semantics), then classify → resolve default branch → ensure history → select base.

### `CiEnvironment` — the only place env vars are read

A plain struct populated from environment variables, passed by value so every downstream function is pure and table-testable. Captured variables:

- **Buildkite:** `BUILDKITE`, `BUILDKITE_PULL_REQUEST` (`"false"` or a number), `BUILDKITE_PULL_REQUEST_BASE_BRANCH`, `BUILDKITE_BRANCH`, `BUILDKITE_COMMIT`, `BUILDKITE_PIPELINE_DEFAULT_BRANCH`.
- **GitHub Actions:** `GITHUB_ACTIONS`, `GITHUB_EVENT_NAME` (`pull_request` / `push` / `merge_group`), `GITHUB_BASE_REF`, `GITHUB_HEAD_REF`, `GITHUB_REF`, `GITHUB_SHA`, `GITHUB_EVENT_PATH` (path to the event JSON, read lazily only when we need `merge_group.base_sha`/`head_sha` or `repository.default_branch`).
- **Generic:** `CI`.

No other code in checkleft reads these; production uses `CiEnvironment::from_env()`, tests construct `CiEnvironment` literals directly. All event-payload fields are treated as optional with fallbacks (see the matrix), and the payload parser is regression-tested against vendored real-shaped event JSON fixtures.

### Scenario classification (the centralised decision)

`classify(env, default_branch) -> Scenario` is a pure function and the single source of truth for the PR-vs-merge-queue distinction. One signature change from the original sketch: classification takes the **already-resolved default branch**, because distinguishing push-to-default from push-to-branch requires knowing what the default is — so default-branch resolution runs _before_ classification, not after. Precedence is explicit and ordered so ambiguous combinations are deterministic:

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

This is the heart of the design. Every row is a unit test (env literal → expected `Scenario`, stub prober → expected base) and an e2e test (real git topology → expected base + scoped files). The matrix below is the **as-built** version; rows 1, 3, 4, and 5 evolved during implementation, as noted.

| #   | Scenario                             | Detection signals                                                        | Base revision                                                                                                              | Diff semantics                       | Notes / bug guarded                                                                                                                                                                                                                                                                                                   |
| --- | ------------------------------------ | ------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1   | **Regular PR**                       | GHA `pull_request`, or BK `BUILDKITE_PULL_REQUEST != false`              | `merge-base(origin/<base-branch>, HEAD)`, falling back to the local branch name if the remote-tracking ref doesn't resolve | 3-dot equiv (diff `mergebase..HEAD`) | Base branch = `GITHUB_BASE_REF` / `BUILDKITE_PULL_REQUEST_BASE_BRANCH`, else default branch. **Must NOT 2-dot against `origin/base`** (T843/#948). Prefers `origin/<branch>` because Buildkite agents reuse checkout directories and a stale local `main` mis-scopes (bake finding, #1104).                           |
| 2   | **GitHub merge queue**               | GHA `merge_group`                                                        | `merge_group.base_sha` from event payload, else `HEAD^1`                                                                   | 2-dot `base..HEAD`                   | base_sha == the main tip being merged onto. **Must NOT `merge-base(HEAD^1,HEAD^2)`** (T774/#910).                                                                                                                                                                                                                     |
| 3   | **Buildkite merge queue**            | `BUILDKITE_BRANCH` = `gh-readonly-queue/<branch>/...`                    | `HEAD^1` **unconditionally**                                                                                               | 2-dot `HEAD^1..HEAD`                 | The originally designed "verify HEAD is a merge commit via `HEAD^2`, else fall back to the PR rule" was removed: in shallow checkouts `HEAD^2` is often unfetched even for genuine merge commits, so the sentinel mis-fired into the fork-point failure mode (T1016/#1104).                                           |
| 4   | **Push to default** (main/master)    | push event, branch == default                                            | `HEAD^1`                                                                                                                   | `parent..HEAD`                       | The design's optional `before..after` range (using CI's `before` sha) was dropped: `before` is all-zeros on branch creation and unreachable after force-push, and pushes to default here are merge-queue merge commits where `HEAD^1` is exactly right. Left as a possible future enhancement (comment in `base.rs`). |
| 5   | **Push to non-default branch**       | push event, branch != default                                            | `merge-base(origin/<default-branch>, HEAD)`, same local-name fallback as row 1                                             | 3-dot equiv                          | Treated like a pre-merge branch; same rule (and same stale-local-branch fix) as row 1 without a PR number.                                                                                                                                                                                                            |
| 6   | **Local / pre-push**                 | no CI env                                                                | `merge-base(default-branch, HEAD)`                                                                                         | 3-dot equiv                          | For **git**, the scope is the committed `base..HEAD` diff — uncommitted/staged changes are _not_ included (see F4). For **jj**, the working copy `@` is a real commit, so working-copy changes are naturally included.                                                                                                |
| 7   | **First commit / no merge-base**     | merge-base computation yields nothing (root commit, unrelated histories) | —                                                                                                                          | —                                    | `ChangePlan::Empty { NoMergeBase }` → check nothing (or `--all` if operator opts in). Logged clearly, exit 0.                                                                                                                                                                                                         |
| 8   | **Detached HEAD**                    | `HEAD` not on a branch and no CI hint                                    | best-effort: `HEAD^1` if it exists, else `Empty { DetachedHeadNoParent }`                                                  | parent..HEAD                         | Common in some CI checkouts; never errors hard.                                                                                                                                                                                                                                                                       |
| 9   | **Forced shallow, base unreachable** | shallow repo, base ref not in local history                              | deepen/unshallow, then recompute; if still unreachable → clear error                                                       | —                                    | See "Shallow handling".                                                                                                                                                                                                                                                                                               |

**The opposite rules (rows 1 vs 2/3) live in exactly one place** — `base.rs::select_base` matching on `Scenario`. There is no shell `if`, and the two rules are adjacent and commented so the asymmetry is impossible to miss.

### Default-branch resolution

`resolve_default_branch(env, prober, override)` tries, in order, and returns the first that resolves to an existing ref:

1. Explicit `--default-branch` override.
2. CI hint: `BUILDKITE_PIPELINE_DEFAULT_BRANCH`; the `<target>` segment of a `gh-readonly-queue/<target>/...` ref; GHA `repository.default_branch` from the event payload.
3. `git symbolic-ref refs/remotes/origin/HEAD` (the remote's default branch) → strip to short name.
4. Probe `origin/main` then `origin/master` (then local `main`/`master`) and pick whichever exists.
5. Fallback: `main`, with a warning.

This satisfies requirement 2 (main _or_ master, detected not hardcoded). Ref existence is probed through the `RefProber` trait, so the whole ladder is unit-tested without a real repo; `GitRefProber` is the production implementation.

### Shallow handling

CI checkouts are frequently shallow (`git clone --depth=1`), so the base commit / merge-base may not be present locally. `shallow.rs::ensure_history(root, kind, needed_ref, scenario)`:

1. `git rev-parse --is-shallow-repository`. If false, done — a non-shallow repo never triggers a fetch.
2. If shallow, make the base reachable with bounded work, split by what the scenario actually needs:
   - **Merge-queue / push-to-default** only need `HEAD^1`: a single `git fetch --deepen=1` suffices.
   - **PR / push-to-branch / local** need a merge-base against `needed_ref`: `git fetch --deepen` in increasing steps (50 → 250 → 1000) with a `git merge-base` reachability re-test after each, then `git fetch --unshallow origin` as the last resort. (Push-to-branch was originally grouped with the `deepen=1` cases — wrong, since row 5 computes a merge-base, not `HEAD^1`; regrouped during the bake, #1104.)
3. If after unshallow the base is _still_ unreachable (e.g. base branch was never fetched), emit a precise error: which ref, what we tried, and the one-line remedy (fetch the base branch / increase clone depth). Never silently fall back to diffing against the tip — that is the failure mode that produced the original bugs.

The `needed_ref` handed to `ensure_history` for PR and push-to-branch scenarios is `origin/<branch>` (not the bare branch name), so deepening fetches from the remote and `select_base` then sees a fresh remote-tracking ref — mirroring the `git fetch origin main` the legacy `checks.sh` always did.

For **jj** colocated repos the underlying git repo is what is shallow. Detection: if `Vcs` is `Jujutsu` but a colocated `.git` exists, shallow operations shell out to `git` in the repo root, then run `jj git import` to sync jj's op log with the deepened history (the design's open question Q3, answered conservatively: yes, the import step is needed).

### jj vs git handling

- **VCS detection** is unchanged (`Vcs::detect` already prefers jj, falls back to git).
- **Changed-file set** reuses `changeset_since` / `current_changeset`, which already branch on `VcsKind`.
- **Base selection** is computed in git terms (merge-base, `HEAD^1`, shas) because CI topology _is_ git. In a jj repo we translate: a resolved git sha is a valid jj revision (jj accepts git commit ids), so the sha in `ChangePlan::Scoped` works for `jj diff --from <sha>`. This keeps one base-selection code path rather than two parallel jj/git matrices.
- Requirement 4 ("in a jj repo it may shell out to git") is satisfied: scenario classification and base _resolution_ use git; the _diff_ uses whichever VCS is native.

### Self-sufficiency API surface

What `checkleft run` resolves on its own vs. optional overrides:

| Input                     | Source                                      | Default behaviour                                                                                                  |
| ------------------------- | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Scenario                  | env (`CiEnvironment`)                       | auto-classified; Local when no CI signal                                                                           |
| Default branch            | CI hint → `origin/HEAD` → probe main/master | auto                                                                                                               |
| Base revision             | scenario→base matrix                        | auto                                                                                                               |
| Shallow depth             | auto-deepen as needed                       | auto                                                                                                               |
| `--all`                   | operator flag                               | off (scope to changes)                                                                                             |
| `--base-ref=<ref>`        | operator flag                               | unset; when set, **bypasses classification** and uses `merge-base(ref, HEAD)` as before (back-compat escape hatch) |
| `--default-branch=<name>` | operator flag                               | unset; overrides resolution step 1                                                                                 |

The steady state in CI is literally `checkleft run` (plus existing `--external-checks-*` config) — this is what `.buildkite/steps/checks.sh` now invokes. `--base-ref` is retained only for humans debugging or for an exotic context we haven't enumerated. The escape hatches are documented as such in `tools/checkleft/README.md`.

A temporary `checkleft show-plan` subcommand (prints the resolved `base_sha`, `changed_files` count, and `scenario` without running checks) was added for the migration bake. It is marked for removal now that the bake is over but is still present in `main.rs` — a leftover cleanup, tracked as follow-up work.

### Migration: how the `checks.sh` scoping logic was retired

The migration was staged so there was never a window where CI mis-scoped, and executed exactly as planned:

1. **Land the checkleft change** (PRs #1085 classification core, #1090 base matrix, #1094 shallow handling, #1099 orchestration + wiring into `main.rs`). `--base-ref` stayed honoured exactly as before, so the existing `checks.sh` kept working unchanged through this whole phase.
2. **Bake in CI** (PR #1104): a temporary `checks-bake (TEMP)` pipeline step ran `checkleft show-plan --base-ref=<legacy sha>` (legacy path, sha derived by mirroring `checks.sh`) and `checkleft show-plan` (auto path) side by side and hard-failed on any base-sha divergence across the three live scenarios (PR build, `gh-readonly-queue/*`, push-to-main). **The bake earned its keep**: it surfaced the T1016 `HEAD^2` sentinel bug and the stale-local-branch problem on reused Buildkite checkouts, both fixed in #1104 itself before any shell logic was removed.
3. **Shrink `checks.sh`** (PR #1121): removed the entire `if/elif/else` base-computation and `--unshallow` blocks; the step is now repobin install + `bin/checkleft run`. This retired the T774/T843/#948/#910/#945 shell logic in one commit, and also removed the bake scaffolding (`checks-bake.sh`, pipeline step) plus `checks_pr_base_test.sh` / `checks_merge_queue_base_test.sh` — shell tests for logic that no longer exists (a small scope addition vs. the original plan, which hadn't accounted for them).
4. **Document** (also #1121): `tools/checkleft/README.md` states change detection is automatic, `bin/checkleft run` is the complete CI invocation, and `--base-ref`/`--all`/`--default-branch` are escape hatches only.

### Observability

`checkleft run` logs (at info, via `tracing`) the classified scenario, the resolved default branch, deepen/unshallow events, and the final base sha + changed-file count. This replaced the `echo "[checks] ..."` breadcrumbs in `checks.sh`, so CI logs remain debuggable now that the shell has shrunk.

### Testing (as shipped)

- **Unit tests** (PRs #1085, #1090, #1094, #1099, #1104): table-driven coverage of every classification precedence case, every default-branch ladder step, and every matrix row via stub probers — no real repository needed. The bake-period fixes landed with their own regression tests (e.g. `row1_pr_prefers_origin_ref_over_stale_local`).
- **e2e suite** (PR #1107, `tools/checkleft/tests/change_detection_e2e.rs`): one named test per matrix row, each building an isolated _real_ git repo (fork, base-drift commits, synthetic queue merge commits, shallow clones via `git init` + `fetch --depth=1` — local-path `clone --depth` is silently ignored by git — and a jj-colocated variant that skips when `jj` is not on PATH). Each asserts both the resolved base sha and the scoped file set through the public `resolve_change_plan` → `changeset_since` path, exactly as the binary drives it.
- **Bug-history regressions**: T774/#910, T843/#948, #945, and b12d4ede are encoded as named tests that fail if any reappears (e.g. the merge-queue test plants an unrelated `github_oauth.rs`-shaped file at the queue base and asserts it is not swept in).
- **Event-payload fixtures**: real-shaped GitHub `merge_group` and `pull_request` event JSONs are vendored under `tools/checkleft/tests/fixtures/` and parsed through the production `read_github_event_payload` path — this delivered deferred item F3 ahead of schedule.

## Resolution of the original open questions

1. **GitHub event payload parsing.** All payload fields are optional with `HEAD^1` / ref-probe fallbacks, and real payload fixtures are vendored (PR #1107). Resolved; F3 delivered.
2. **Auto-deepen cost.** Shipped with the fixed 50/250/1000 ladder; no repo hit the cap during the bake. Depth remains non-configurable until someone needs it (F2 stands).
3. **jj + shallow interaction.** Confirmed: jj needs an explicit `jj git import` after the colocated git repo is deepened; `shallow.rs` does this (PR #1094).
4. **Local working-tree scope semantics.** Resolved conservatively: for git, local runs scope to the committed `merge-base..HEAD` diff only — uncommitted/staged changes are not included. jj local runs include the working copy naturally (`@` is a commit). Including uncommitted changes for git remains an open operator decision (F4).
5. **Detached HEAD.** `HEAD^1` (check the last commit) was chosen over `Empty` as the ambiguous-case default, with `Empty { DetachedHeadNoParent }` only when there is no parent at all.
6. **Back-compat of `--base-ref`.** Preserved exactly (idempotent `merge-base(ref, HEAD)`); the bake's parity comparison depended on it and it remains the documented escape hatch.
7. **Push-to-default range.** Resolved: `HEAD^1` always. The `before..after` range was dropped as unreliable (zero sha on branch creation, unreachable after force-push); pushes to default in practice are merge-queue merge commits, for which `HEAD^1` is exact.

## Implementation record

The work shipped as seven PRs, closely following the planned task breakdown:

| Task  | Scope                                                                                                                                                                                                                | PR    |
| ----- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----- |
| T1–T3 | `CiEnvironment`, `Scenario` + `classify`, default-branch resolver                                                                                                                                                    | #1085 |
| T4    | `base.rs` scenario→base matrix (`select_base`, `HeadProber`)                                                                                                                                                         | #1090 |
| T5    | `shallow.rs` bounded deepen/unshallow + precise error                                                                                                                                                                | #1094 |
| T6    | `resolve_change_plan` orchestration, wired into `main.rs`; `--default-branch` flag; observability                                                                                                                    | #1099 |
| T9    | Temporary parallel-run parity bake (`show-plan`, `checks-bake.sh`, pipeline step) + the two bake-surfaced fixes (T1016 `HEAD^2` sentinel, stale-local-branch `origin/` preference, `PushToBranch` deepen regrouping) | #1104 |
| T7    | e2e/functional suite across all matrix rows + bug-history fixtures + vendored event payloads (F3)                                                                                                                    | #1107 |
| T8    | Shrink `checks.sh` to `checkleft run`; remove bake scaffolding and obsolete shell tests; README docs                                                                                                                 | #1121 |

Known leftover: the temporary `show-plan` subcommand (T9 scaffolding, marked "remove once checks.sh scoping is retired" in `main.rs`) was not removed by #1121 and should be cleaned up.

### Deferred (`future / not a v1 blocker`)

- **F1. Pluggable CI-provider registry.** Generalising `CiEnvironment` into a provider plugin system for CI systems beyond Buildkite/GHA — add only when a third provider appears.
- **F2. Configurable deepen depth / cap via env or config.** Shipped with the fixed ladder; make it configurable only if a repo hits the cap.
- **F4. Uncommitted/staged-change inclusion for local git runs.** Pending operator decision; current behaviour scopes local git runs to committed changes only. (The related `before..after` push range from Q7 is likewise parked, noted as a future enhancement in `base.rs`.)
- **F5. Submodule / subtree-aware scoping.** Explicitly out of scope (non-goal).
- ~~**F3. Vendored corpus of real GitHub event payloads.**~~ Delivered in #1107.
