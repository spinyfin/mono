# flunge's pinned checkleft alpha.100: push-gate base selection and the CHECKS_PR_NUMBER path

- **Date:** 2026-07-24
- **Repo / revision:** `spinyfin/mono` at `main@origin` = `815a64ad`; `brianduff/flunge` at `main`
- **Parent project:** "Boss-ism leakage: generic regex check in checkleft, adopted by boss and flunge"
- **Design doc:** [`tools/boss/docs/designs/boss-ism-leakage-generic-regex-check-in-checkleft-adopted-by-boss-and-flunge.md`](../../tools/boss/docs/designs/boss-ism-leakage-generic-regex-check-in-checkleft-adopted-by-boss-and-flunge.md)
- **Scope:** written finding only; no code change.

## Verdict

flunge's pinned `checkleft-v0.1.0-alpha.100` **does** carry the `select_base_local` origin-preferring fix, and the `CHECKS_PR_NUMBER` PR-description path **does** work on that binary in flunge's CI configuration. Both were confirmed by running the actual pinned binary, not only by reading source.

There is therefore **no urgency signal to fold into the flunge pin bump**. The bump is not repairing a broken push gate; it is purely the delivery vehicle for the new check, and it can stay sequenced exactly where the design doc already puts it — after the checkleft release.

Two adjacent limitations did surface, and they matter to the project's later adoption steps rather than to the pin bump itself: the PR-description path is inert inside cube worker workspaces, and flunge's CI skips checkleft entirely for PRs that touch only frontend, mobile, or docs. Both are detailed below.

## Questions asked

1. Does flunge's pinned checkleft `alpha.100` contain the `select_base_local` origin-preferring fix seen at `tools/checkleft/src/change_detection/base.rs:298-317` on mono HEAD?
2. Does the `CHECKS_PR_NUMBER` → PR-description path work on that pinned version, in flunge's actual CI wiring?

## Method

Three independent lines of evidence were used, so a conclusion never rests on source reading alone:

1. **Provenance.** Resolve flunge's pin to a mono tag, resolve the tag to a commit, and test ancestry of the fix commit against it.
2. **Source identity.** Diff the whole `tools/checkleft/` tree between the tagged commit and mono HEAD, and byte-compare the specific files involved.
3. **Execution.** Run the _actual_ cached `alpha.100` binary that `bin/checkleft-bootstrap.sh` downloads, against purpose-built scratch repositories that reproduce the relevant shapes — a plain git checkout (flunge CI) and a non-colocated jj workspace (a cube worker workspace). A pre-fix release, `alpha.57`, was run against the same fixture as a counterfactual control.

## Finding 1 — alpha.100 carries the fix

flunge pins the same release in both places it records one, and both match `brianduff/flunge@main`:

| File                 | Pin                                                                         |
| -------------------- | --------------------------------------------------------------------------- |
| `REPOBIN.toml`       | `[pins.checkleft] tag = "checkleft-v0.1.0-alpha.100"`                       |
| `bin/checkleft.lock` | `version = "0.1.0-alpha.100"`, `release_tag = "checkleft-v0.1.0-alpha.100"` |

The tag resolves to mono commit `ab96ee1a`, published 2026-07-14. The origin-preferring fix landed in `e8a333ed` ("checkleft: `Scenario::Local` base resolution must prefer `origin/<default-branch>`", #1597) on 2026-06-21, and `e8a333ed` is an ancestor of `ab96ee1a`.

Source identity is stronger than ancestry alone: `change_detection/base.rs` at the tag is byte-identical to mono HEAD, with `select_base_local` at exactly lines 298-317. The full `tools/checkleft/` diff from `ab96ee1a` to `815a64ad` touches 38 files, and **none** of them is under `src/change_detection/`, nor is it `src/main.rs`, `src/vcs.rs`, or anything under `wit/`. Everything relevant to both questions is frozen at HEAD's content.

Running the pinned binary confirms it. In this cube workspace (a secondary jj workspace with no `.git` at its root), `checkleft show-plan` reports:

```text
base_sha=815a64ad871b4512862c71fc28d3d97428c916fd   # = main@origin tip
changed_files=0
scenario=local
```

## Finding 2 — the counterfactual, so the fix's effect is not taken on faith

The hazard the fix addresses is a local `main` bookmark sitting at or ahead of the working commit while only `origin/main` is authoritative. A scratch non-colocated jj workspace was built to that exact shape: `main@origin` at the true base, local `main` moved onto `@`, one oversized file added, and a `file/size` check configured with `max_lines: 10`.

Same fixture, two binaries:

| Binary                           | `base_sha`                 | `changed_files` | `checkleft run`            |
| -------------------------------- | -------------------------- | --------------- | -------------------------- |
| `alpha.57` (2026-06-16, pre-fix) | `ad01f694` — equal to `@`  | **0**           | `No checks ran.`, exit 0   |
| `alpha.100` (flunge's pin)       | `afb35ebb` — `main@origin` | **1**           | `error[file/size]`, exit 1 |

That is the no-op failure mode, reproduced and then shown to be absent on the pinned version. `alpha.57` silently passes a change that `alpha.100` correctly rejects.

## Finding 3 — the CHECKS_PR_NUMBER path works end-to-end on alpha.100

This was proven end-to-end rather than inferred, using a real PR body as the fixture. flunge PR #723 contains a line-leading `BYPASS_FILE_SIZE=…` directive, so a successful fetch is observable as a behaviour change, not just a log line.

A scratch git repo with `origin` set to `git@github.com:brianduff/flunge.git` and a failing `file/size` check was run twice with the pinned `alpha.100` binary:

- Baseline, no PR env: `error[file/size] … exceeding configured max_lines=10`, exit 1.
- With `CHECKS_PR_NUMBER=723`: `warning[file/size]: check was bypassed via BYPASS_FILE_SIZE`, and the bypass reason printed back was the real text from that PR's body.

The trace line confirms the intended resolution order was used:

```text
INFO checkleft: fetching PR description by change id repository="brianduff/flunge" change_id="723"
```

flunge's CI supplies both halves this path needs. `.buildkite/scripts/run_checks.sh` exports `CHECKS_PR_NUMBER="${BUILDKITE_PULL_REQUEST}"` on PR builds, and the generated `:mag: Checks Framework` step declares `GITHUB_TOKEN` among its `secrets`, so `detect_github_token()` finds a token. Repository resolution succeeds because the Buildkite checkout is a plain git working tree.

## Finding 4 — the PR-description path is inert inside cube worker workspaces

`Vcs::remote_repo_slug()` (`tools/checkleft/src/vcs.rs:207-215`) resolves the repository by shelling `git remote get-url origin` unconditionally — there is no jj branch, and `run_command` sets no `GIT_DIR`. A cube workspace is a secondary jj workspace with no `.git` at its root, so that command exits 128 and the slug is `None`. `resolve_pr_description` then returns at its `let repository = repository?;` guard before any network call.

Reproduced with the pinned binary in a non-colocated jj repo: with `CHECKS_PR_NUMBER=723` set, no `fetching PR description` line appears and the check still errors. Adding `CHECKS_REPOSITORY=brianduff/flunge` restores the full behaviour — the fetch happens and the bypass applies. So the capability is present; only repository _discovery_ is missing.

Two consequences worth carrying forward:

- The `cube pr create` / `cube pr update` push gate (`tools/cube/src/app.rs:3366`) documents bypass as "checkleft's own `BYPASS_<CHECK>=<reason>` directives in the commit message / PR description". In a cube workspace only the commit-message half actually works today.
- A changeset-scoped check will see `commit-description` but never `pr-description` at push-gate time. That is not fatal for the project — the editorial hook owns the PR body at author time by design, and CI is the backstop — but the deterministic gate that runs _before the push_ covers strictly less than it appears to.

## Finding 5 — flunge CI skips checkleft entirely for frontend-, mobile-, and docs-only PRs

`.buildkite/scripts/upload_dynamic_pipeline.sh` emits the `:mag: Checks Framework` step only when `need_backend` or `need_cli` is set. Those flags are set by path prefix: `backend/*`; `cli/*`, `tools/flunge-debug/*`, `tools/release-prod`, `tools/release-main`; `REPOBIN.toml`, `REPOBIN.lock`, `tools/checks`, `CHECKS.yaml`, `*/CHECKS.yaml`; and the shared CI set (`rbe/*`, `BUILD`, `MODULE.bazel`, `MODULE.bazel.lock`, `.bazelrc`, `Cargo.toml`, `Cargo.lock`, `.buildkite/*`, `.github/actions/setup-bazel/*`).

A PR touching only `frontend/**`, only `mobile/ios/**`, or only `docs/**` therefore runs **no** checkleft at all. (`run_all=1` when base-ref detection fails is the one escape hatch, and it is a failure path, not a normal one.)

This is the sharpest limit on what adopting the regex check in flunge will actually catch. A boss-ism in a frontend-only PR's description or commit messages would pass CI untouched, because the check is never scheduled. Fixing it is a one-line pipeline change — make the checks step unconditional, or add a catch-all trigger — but it is a flunge CI decision, outside this project's stated scope.

## Finding 6 — a stale reference doc in mono

`tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md:74` describes `run_checks.sh` as invoking `./tools/checks run --base-ref <merge-base> --format=human` (or `--all`), and as setting `CHECKLEFT_BUILD_EXTERNAL_PACKAGES=1` and `CHECKLEFT_EXTERNAL_PROVIDER_MODE=generated-only`.

The current script does none of that. It calls `run_checkleft run --format="${checks_output_format}" --show-progress=false` and relies on checkleft's own scenario classification, with no `--base-ref`, no `--all`, and neither environment variable. The `CHECKS_PR_NUMBER` claim on the same line is still accurate. Anyone sizing the flunge adoption work from that reference will mis-model how base selection happens there.

## What this means for the project's ordering

- **The flunge pin bump carries no urgency premium.** It is not repairing a silently-passing push gate. Leave it sequenced after the checkleft release that contains the new check, exactly as designed, and record "no urgency signal" as the outcome of this question.
- **flunge is 6 releases behind** (latest published is `alpha.106`), but nothing in that gap bears on either question here — the gap is entirely built-in checks, the declarative external-check path, and test reorganisation.
- **Adopting the check in flunge should be paired with a CI-trigger fix**, or its coverage will be silently partial for frontend, mobile, and docs PRs. This is worth deciding before the flunge adoption step, not after.
- **Do not assume the push gate sees the PR description.** Any design that relies on catching a boss-ism in the PR body _before_ the push must lean on the editorial hook; checkleft's PR-body coverage in flunge begins at CI.

## Follow-up code changes (out of scope here, listed for filing)

1. Teach `Vcs::remote_repo_slug()` a jj path (`jj git remote list` → parse `origin`) so repository resolution — and with it PR-description fetch and PR-body bypass — works in non-colocated jj workspaces. Small and self-contained in `tools/checkleft/src/vcs.rs`.
2. Make flunge's `:mag: Checks Framework` step unconditional in `.buildkite/scripts/upload_dynamic_pipeline.sh`, so checkleft runs on every PR rather than only backend/cli-touching ones.
3. Correct `tools/boss/docs/designs/flunge-buildkite-pipeline-reference.md:74` to match the current `run_checks.sh`.

## Open questions

- Should the push gate set `CHECKS_REPOSITORY` itself as a stopgap (cube already resolves `owner/repo` for its `gh` calls) instead of waiting on a jj-aware `remote_repo_slug()`? That would make PR-body bypass work at push time immediately, at the cost of a second place that knows how to name the repo.
- Making flunge's checks step unconditional adds a step to every PR, including docs-only ones. Is that acceptable against its runtime — which includes an `npm ci` for the frontend lint plugins — or should the trigger set merely be widened instead?
