# Boss CI: Buildkite Pipeline Mirroring Flunge

## Status (2026-07-20): shipped and enforcing

The pipeline is live and branch protection on `main` is on. A PR to mono cannot merge while a required buildkite check is red.

- **Delivered by:** PR #555 (flunge pipeline audit — `flunge-buildkite-pipeline-reference.md`), PR #563 (`.buildkite/` skeleton), PR #565 (static checks), PR #567 (test steps), PR #599 (per-step `notify` contexts, `REQUIRED_CHECKS.md`, the `no_gh_pr_merge` grep-guard test, branch-protection enablement).
- **Live required checks** (verified via `gh api .../branches/main/protection`): `buildkite/mono/bazel-build-test`, `buildkite/mono/mac-app-build`, `buildkite/mono/checks`. `strict` is off (no up-to-date-branch requirement), `enforce_admins` is off.
- **As-built shape diverged from the original plan in several places** — no bootstrap step, no cargo or pnpm steps, build+test consolidated into one step, and a macOS build step that the original design had scoped out. Each divergence is covered inline in the section it affects.
- Adjacent pipelines have since grown alongside the PR pipeline (a scheduled `mono-integrity` full-repo build/test, checkleft release pipelines, and a `boss-release` step) — noted under "Beyond the original scope" below.

## Motivating incident

On 2026-05-12, the `#[cfg(test)]` blocks in the engine's `completion.rs` and `merge_poller.rs` drifted out of sync with their prod signatures. `cargo test -p boss-engine --no-run` reported six compile errors on `main`. The drift sat undetected on `main` for roughly twenty-four hours. La Forge's investigation in closed PR #438 (chore `task_18af35a1e855d7f0_24`) tracked down the cause; the doc didn't land on main, but the findings stand.

Why nothing caught it: at the time, `bazel test //tools/boss/engine/...` resolved to only two integration test targets — there was no `rust_test(crate=":engine_lib")`, so bazel silently skipped 561 lib tests. Sibling chore `task_18af3caac58d9748_2c` ("P1") closed that gap; after the engine's crate split, the rule lives at `//tools/boss/engine/core:engine_lib_test`. With no other gate, every dispatched worker was a bet that what landed on `main` since the last green check was still buildable.

This design is the structural fix: a buildkite pipeline for the mono repo, mirroring the shape of the existing flunge pipeline, gating merges on green.

## Goals — all met

- A PR to mono cannot be merged while buildkite reports red. GitHub branch protection enforces it; no engine-side gating required. ✅
- The pipeline runs on every PR push and on pushes to `main`. ✅
- A PR that introduces a bazel build or test failure is blocked from merging. ✅ (The compile-drift class of failure from the motivating incident is now caught by `bazel-build-test` via `engine_lib_test`.)
- Reviewers see a clear pass/fail signal in the GitHub PR UI; the Boss UI surfaces the same signal next to in-review rows (`PrCiIndicator` in `tools/boss/app-macos/Sources/ContentView.swift`, fed by the engine's `ci_watch` probe). ✅
- Same buildkite org and merge-blocking semantics as flunge, diverging where the rust + bazel surface forces it. ✅

## Non-goals

- **Engine-side gating.** The engine does not call `gh pr merge`; the `merge_poller` detects merges, it does not perform them. Branch protection is the only enforcement layer. PR #599 locked this assumption with a grep-guard `sh_test` (now at `tools/boss/engine/core/tests/no_gh_pr_merge_test.sh`) that fails if any `gh pr merge` call appears in engine source.
- **Performance dashboards.** Flunge doesn't have one; nor does this.
- **macOS app code-signing / notarisation CI.** Still out of scope. Note the boundary moved, though: _building and testing_ the macOS app in CI turned out to be necessary and shipped as the required `mac-app-build` step (see below) — only signing/notarisation remains a follow-up.
- **Engine test-suite perf work.** Tracked separately under P2 (`task_18af3cad1705c0a8_2d`).
- **Repobin dispatch-cache.** Landed separately in PR #439.
- **Cross-repo CI orchestration.** This pipeline gates the mono repo only.
- **Rewriting `ci_watch.rs`.** The engine already modelled buildkite as a CI provider; once real checks existed, the existing remediation pipeline picked them up without changes, as predicted.

## Alternatives considered

### Alternative A — GitHub Actions instead of Buildkite

GitHub Actions is the obvious "free" option: no infra to stand up, no agent fleet to manage, native PR integration. Rejected:

- Flunge runs on buildkite. Mandate was _mirror flunge_, not _re-pick the CI vendor_.
- The mono build is rust + bazel + Xcode. Bazel cache hit rate matters — GitHub-hosted runners are ephemeral, so a remote cache would be mandatory just to be tolerable. Self-hosted GH runners would re-introduce the agent-fleet question without buying anything over buildkite.
- We already had buildkite secrets, an org, an agent fleet, and the engine knows how to parse `buildkite.com` job URLs (`ci_watch`). Reusing that surface was materially cheaper than building a parallel actions parser.

This held up in practice; nothing in the rollout suggested revisiting the vendor choice.

### Alternative B — install-on-boot toolchains vs pre-provisioned agents

The original design proposed a `bootstrap.sh` step that would install/pin the rust toolchain, bazelisk, and node tooling at the start of every job, so any generic agent could run mono jobs. **As built, we converged on flunge's model instead: agents are pre-provisioned** with the toolchains they need, and there is no bootstrap step. The skeleton's placeholder `bootstrap.sh` never acquired real logic and was removed as dead code in PR #1291. The flunge audit (PR #555) had flagged this exact fork in the road ("there isn't a bootstrap surface in-repo — agents are pre-provisioned"); mono ended up on the same side.

### Alternative C — `cargo` only, drop `bazel` from CI

Rejected, and in fact the outcome inverted it: **bazel-only, no cargo**. The original v1 kept `cargo check --workspace` as a cheap fast-failing compile guard alongside bazel (and PR #565 wired it), but it was dropped the same day (2026-05-15, commit `8ef4895b`) before checks were ever promoted to required. `bazel build` catches everything cargo-check would, P1 made `bazel test` the canonical rust test signal, and a second build system in CI was pure wall-clock and cache-pressure cost. Bazel is the single build/test authority in CI; per repo policy it is also the only sanctioned local path (no bare `cargo`).

## As-built pipeline

### Repo layout

```
.buildkite/
  pipeline.yml                       # PR/main pipeline: steps, queues, notify contexts
  pipeline-integrity.yml             # scheduled full-repo integrity pipeline (~30 min cadence)
  pipeline-checkleft-release.yml     # checkleft release automation
  pipeline-checkleft-release-builds.yml
  steps/
    ci-env.sh                        # shared CI env: --config=ci-<os>, bazel startup flags, Xcode-drift recovery
    bazel-build-test.sh              # bazel build //... then bazel test //... (one step, one agent)
    mac-app-build.sh                 # bazel build+test of the macOS app + installer targets
    checks.sh                        # CHECKS.yaml runner via checkleft
    boss-release.sh, checkleft-release.sh, integrity-*.sh
  README.md
  REQUIRED_CHECKS.md                 # authoritative required-check list + rename contract
```

As designed, `pipeline.yml` declares steps, queues, and notify blocks only — all logic lives in reviewed, versioned `steps/*.sh` scripts, matching flunge.

### Pipeline shape

Three parallel gating steps, no barrier between them, plus a main-only release step:

```
┬──► bazel-build-test (build, then test, one agent) ──┐
├──► mac-app-build                                    ├──► boss-release (main only)
└──► checks                                           ┘
```

Two structural changes from the designed shape (`bootstrap → [static checks] → wait → [tests]`):

- **No bootstrap step** — see Alternative B; agents are pre-provisioned.
- **Build and test are one step, and the cross-step wait barrier is gone.** The design ran `bazel build` and `bazel test` as separate buildkite steps so a red step was unambiguous. In practice the `bazel-any` queue mixes darwin and linux hosts, so a test step was not guaranteed to land on the agent that had just built — re-analyzing and rebuilding from scratch. PRs #1889/#1896/#1924 consolidated them into a single `bazel-build-test` step that runs `bazel build //...` then `bazel test //...` back to back on one agent, with per-phase collapsible log groups (`[bazel-build]` / `[bazel-test]`) preserving failure attribution inside the step. The rename to a new required-check context was ramped safely: the step emitted the two legacy contexts plus the new one during the transition, an operator flipped `required_status_checks` on 2026-07-13, then the legacy notify entries were removed. `REQUIRED_CHECKS.md` records the full sequence and the generalized rename contract.

Step details:

- **`bazel-build-test`** excludes `//tools/boss/app-macos/...` and `//tools/boss/installer/...` (Swift; no Linux toolchain) — those belong to `mac-app-build`. Runs with `--keep_going` so one broken target doesn't mask others.
- **`mac-app-build`** — not in the original design (macOS CI was scoped out wholesale). It became load-bearing because the repo's build surface is genuinely rust + Swift: the macOS app and the installer's `boss_pkg_payload` rule can only build on a macOS agent, and app regressions were otherwise invisible to CI. It bazel-builds and bazel-tests all `app-macos` and `installer` targets on the `macos-arm64` queue and is a required check.
- **`checks`** — runs the `CHECKS.yaml` suite via `bin/checkleft run` (repobin), auto-scoped to changed paths on PR builds. Invoked via repobin rather than `bazel run` because `bazel run` sets cwd to the runfiles tree, which breaks checkleft's config discovery — resolving old open question R5.
- **`boss-release`** (added later, outside this project's PRs) — main-branch-only, schedule/UI/API-triggered, depends on all three gating steps, so a release only cuts from a fully green commit.

### The node/pnpm surface that wasn't

The original design (and PRs #563/#565/#567) assumed a "rust + node" repo and wired `pnpm-typecheck` and `pnpm-test` steps, with a soft-fail ramp planned for `pnpm-test`. The mono repo has no node surface — no `package.json`, no `pnpm-workspace.yaml` — so the steps were vacuous and were dropped along with `cargo-check` on 2026-05-15 (commit `8ef4895b`), before promotion to required. The soft-fail/advisory ramp machinery designed for flaky steps was therefore never needed: every surviving step went straight to required.

### Required checks for branch protection

Live on `main` (three, not the five PR #599 initially enabled — `bootstrap` disappeared with the step, `bazel-build` + `bazel-test` merged):

- `buildkite/mono/bazel-build-test`
- `buildkite/mono/mac-app-build`
- `buildkite/mono/checks`

Two contract decisions from the rollout, both in `.buildkite/REQUIRED_CHECKS.md` (which shipped as this design proposed, answering open question Q2):

- **Per-step contexts, not flunge's single umbrella check.** Flunge gates on one `buildkite/flunge-ci` context; mono gates per step, so the GitHub PR UI names the failing step directly and `ci_watch` can route by check name.
- **Contexts are pinned explicitly.** Each gating step carries a `notify: github_commit_status: { context: "buildkite/mono/<step-key>" }` block (PR #599), decoupling the context name from the step `label:` (which carries emoji and may change freely). Renames follow the four-step contract in `REQUIRED_CHECKS.md` — update notify + table + branch protection in lockstep, verify the new context posts before dropping the old.

`strict` (require branch up to date) is off, matching flunge. `enforce_admins` is off — flunge has it on; mono deliberately leaves an operator escape hatch.

### Agent topology

The design proposed a single shared `linux-amd64` queue. As built there are two queues, because the Swift surface forces macOS hardware:

- `queue=bazel-any` — mixed darwin/linux hosts; runs `bazel-build-test` and `checks` (neither cares about OS).
- `queue=macos-arm64` — macOS arm64 agents (currently Zakalwe-1); runs `mac-app-build` and `boss-release`.

Both are env-overridable (`BUILDKITE_ANY_QUEUE` / `BUILDKITE_MACOS_QUEUE`) so new agents can be smoke-tested against a real pipeline without touching production queues.

### Bazel cache and daemon hygiene

The design's two-tier plan (v1 disk cache, remote cache as fast-follow) shipped its v1 half; **there is still no remote cache**, and nothing so far has hit the triggers that would justify one. The as-built disk story is more developed than the design sketch:

- Per-OS cache roots on dedicated SSDs, set in `.ci.bazelrc`: `--disk_cache` + `--repository_cache` under `/mnt/ssd/bazel/` (linux) and `/Volumes/ssd/bazel/` (darwin), selected via `--config=ci-linux` / `--config=ci-darwin` from `.buildkite/steps/ci-env.sh`.
- Cache GC tuned for CI (`3T` / `60d` / 5-minute idle delay) vs the conservative local defaults in `.bazelrc`.
- Per-pipeline bazel daemon idle TTLs (`ci-env.sh`): the hot PR pipeline keeps a 2-hour warm daemon; low-cadence pipelines (integrity, checkleft-release) get 15 minutes so their daemons free memory. All CI bazel entry points read startup flags from one `CI_BAZEL_STARTUP_FLAGS` source of truth, because divergent startup flags spawn duplicate daemons.
- Darwin agents pin the Xcode toolchain (`--xcode_version` + `--repo_env=DEVELOPER_DIR`) after two incidents of in-place Xcode updates corrupting cached toolchain state; `ci-env.sh` also detects Xcode-version drift and auto-expunges the stale output base.

Remote cache remains the documented fast-follow, triggered if cold-agent wall-clock becomes painful or the fleet grows enough that inter-agent sharing matters.

### Sharding

None, as designed. `engine_lib_test` is a single `rust_test` using bazel's native test sharding across cores; the suite is comfortably inside the wall-clock budget. Re-evaluate if the slowest required step exceeds ~15 minutes warm.

### Boss UI integration

`PrCiIndicator` (`tools/boss/app-macos/Sources/ContentView.swift`) renders the CI badge on Review-lane kanban cards: yellow clock for `in_progress`, green check for `success`, red X for `fail` (tooltip lists failing check names), hidden for `unknown` so "no signal yet" doesn't read as green. It is fed by the engine's existing `ci_watch` probe (`gh pr view --json statusCheckRollup`); no new schema column was needed. This shipped ahead of the pipeline itself and picked up the real checks automatically once they existed.

### Engine interaction (`ci_watch` remediation)

`ci_watch` acts only on **required** check failures, so the per-step promotion ramp doubled as the flake-storm control: nothing was required until it was stable. The per-PR remediation budget defaults to 3 fix attempts, configurable per-PR (`tasks.ci_attempt_budget`) or per-product (`products.ci_attempt_budget`); the `auto_pr_maintenance_disabled` product flag and per-PR opt-out label remain the circuit breakers. Documented in `REQUIRED_CHECKS.md` §ci_watch.

The jj-vs-git concern (old risk R3) resolved as predicted: buildkite checks out via git, no `jj` exists on agents, and no step or check invokes it — `checkleft` auto-detects git from the working directory.

## Beyond the original scope

The `.buildkite/` directory now hosts more than the PR gate this design covered. Listed for orientation; none of these were deliverables of this project:

- **`mono-integrity` pipeline** — scheduled (~every 30 min) full-repo `bazel build+test` + full checkleft run on main, catching whole-repo rot that PR-scoped runs can miss, plus a commit-delta check.
- **Release pipelines** — `boss-release` (app release from green main) and the checkleft release pipelines (tagging, musl cross-compilation, changelog generation).

## Follow-up state

1. ~~Audit flunge's buildkite pipeline~~ — **done**, PR #555 (`flunge-buildkite-pipeline-reference.md`).
2. ~~Land `.buildkite/` skeleton~~ — **done**, PR #563.
3. ~~Wire static checks~~ — **done**, PR #565 (cargo-check and pnpm-typecheck subsequently removed; see above).
4. ~~Wire test steps~~ — **done**, PR #567 (pnpm-test subsequently removed).
5. ~~Promote checks to required; enable branch protection~~ — **done**, PR #599 + operator enablement; re-ramped through the 2026-07-13 `bazel-build-test` rename.
6. ~~Surface CI status in Boss UI~~ — **done** (`PrCiIndicator`).
7. ~~Post-P1 cargo-test cleanup~~ — **moot**; pipeline shipped bazel-only.
8. Bazel remote cache — **still open as a conditional fast-follow**; no trigger condition met yet.
