# Flunge Buildkite Pipeline — Reference

Audit of flunge's `.buildkite/` setup, captured for the engineer who will land mono's `.buildkite/` skeleton (Boss CI #2). Source: `~/Documents/dev/flunge/.buildkite/` and the live GitHub branch-protection config on `brianduff/flunge`.

The parent design doc (`boss-ci-buildkite-pipeline-mirroring-flunge.md`) describes the _target_ mono pipeline. This doc describes the _current_ flunge pipeline. Where the two disagree, see "Where mono should diverge" at the bottom — those gaps are the load-bearing decisions for #2.

## TL;DR

Flunge does not check in a static "list of steps". It checks in **one upload step** that runs a shell script (`upload_dynamic_pipeline.sh`) which emits a dynamic pipeline based on which paths changed in the PR. Branch protection then gates on **one rolled-up check** (`buildkite/flunge-ci`), not per-step required checks.

That's the headline divergence vs. what the mono design currently sketches.

## Top-level pipeline stub

Six lines, now at `infrastructure/buildkite/pipelines/flunge-ci.stub.yml` (moved out of `.buildkite/`; there is no `.buildkite/pipeline.yml` in the repo today):

```yaml
steps:
  - label: ":pipeline: Upload CI Pipeline"
    key: "upload-ci-pipeline"
    command: ".buildkite/scripts/upload_dynamic_pipeline.sh"
    agents:
      queue: "bazel-any"
```

Buildkite reads this on every trigger. The upload step does the real work: detect changed paths, write a temp pipeline file, run `buildkite-agent pipeline upload`. Buildkite then runs the emitted steps as if they had been static.

## `upload_dynamic_pipeline.sh` — the dispatcher

Computes four booleans — `need_backend`, `need_cli`, `need_frontend`, `need_ios` — from `git diff --name-only <merge-base> <commit>`:

| Path pattern                                                                                                                                   | Flips on                                            |
| ---------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------- |
| `backend/*`                                                                                                                                    | `need_backend`                                      |
| `cli/*`, `tools/flunge-debug/*`, `tools/release-prod`, `tools/release-main`                                                                    | `need_cli`                                          |
| `REPOBIN.toml`, `REPOBIN.lock`, `tools/checks`, `CHECKS.yaml`, `*/CHECKS.yaml`                                                                 | `need_cli` (checks-framework runs under `need_cli`) |
| `rbe/*`, `BUILD`, `MODULE.bazel`, `MODULE.bazel.lock`, `.bazelrc`, `Cargo.toml`, `Cargo.lock`, `.buildkite/*`, `.github/actions/setup-bazel/*` | both `need_backend` and `need_cli` (shared infra)   |
| `frontend/*`, `.buildkite/*`                                                                                                                   | `need_frontend`                                     |
| `mobile/ios/*`                                                                                                                                 | `need_ios`                                          |

If base-ref detection fails (first commit, shallow clone, etc.), all four are set — fail safe by running everything.

Steps the dispatcher can emit:

| Step key               | When                       | Queue                                             | Depends on         | Script                                                      |
| ---------------------- | -------------------------- | ------------------------------------------------- | ------------------ | ----------------------------------------------------------- |
| `checks-framework`     | `need_backend OR need_cli` | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | —                  | `run_checks.sh`                                             |
| `backend-bazel-tests`  | `need_backend`             | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | `checks-framework` | `run_backend_bazel_tests.sh`                                |
| `cli-bazel-tests`      | `need_cli`                 | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | `checks-framework` | `run_cli_bazel_tests.sh`                                    |
| `frontend-tests`       | `need_frontend`            | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | —                  | `run_frontend_tests.sh`                                     |
| `frontend-bazel-tests` | `need_frontend`            | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | —                  | `run_frontend_bazel_tests.sh` (`bazel test //frontend/...`) |
| `ios-bazel-build`      | `need_ios`                 | `macos-arm64` (`BUILDKITE_MACOS_QUEUE` overrides) | —                  | `run_ios_bazel_build.sh`                                    |
| `no-ci-steps`          | nothing else fires         | `bazel-any` (`BUILDKITE_ANY_QUEUE` overrides)     | —                  | inline `echo`                                               |

Notes:

- `checks-framework` is a fan-out point: backend and CLI bazel tests both wait on it.
- `frontend-tests` and `ios-bazel-build` are independent of the bazel chain — they run in parallel with everything else.
- The `no-ci-steps` step exists so the umbrella `buildkite/flunge-ci` check still goes green for, say, a docs-only PR; without it the check would never report and merge would block forever.

No buildkite plugins are referenced in `pipeline.yml` itself. Plugins appear only via the iOS cache block emitted dynamically.

## `lib.sh` — shared helpers

Sourced by every step script. Three exported behaviours worth knowing:

- `ci_repo_root` — `cd` to the repo root regardless of where buildkite drops the working dir.
- `is_pr_build` — true iff `BUILDKITE_PULL_REQUEST` is set to something other than `false`.
- `detect_base_ref` / `changed_files_since` — PR builds resolve to `git merge-base origin/<base> <commit>`; non-PR builds fall back to `HEAD^`. Used both for the dispatcher's path filtering and for `checks.sh` to scope to changed paths.
- `buildkite_bazel_args` + `run_bazel_with_buildkite_args` — wrap `bazel <cmd>` with `--config=ci-linux` on Linux or `--config=ci-darwin` on Darwin (both defined in `.ci.bazelrc`). Remote execution/`--config=remote` is **not** activated by this wrapper; both platforms run bazel locally with disk-cache only. `buildkite_bazel_startup_args` separately points Linux's bazel output root at a fast SSD volume on specific hosts (`zoologist`/`empiricist`), gated on a buildkite agent host tag.

## Per-step scripts

### `run_checks.sh`

`npm ci --prefix frontend` (so checkleft's JS/lint check can resolve ESLint plugins from the project's `node_modules`), then `run_checkleft run --format="${CHECKS_OUTPUT_FORMAT:-human}" --show-progress=false`, where `run_checkleft` invokes checkleft via `repobin exec checkleft` when `repobin` is on the agent, falling back to the prebuilt `bin/checkleft` binary otherwise. There is no `--base-ref` or `--all` flag and no `CHECKLEFT_BUILD_EXTERNAL_PACKAGES` / `CHECKLEFT_EXTERNAL_PROVIDER_MODE` env vars — checkleft classifies the run environment itself. On PR builds the script still exports `CHECKS_PR_NUMBER=$BUILDKITE_PULL_REQUEST` so checks can post inline comments. Runs `decrypt_backend_config.sh` only in `run_backend_bazel_tests.sh`, not here.

### `run_backend_bazel_tests.sh`

1. `decrypt_backend_config.sh` (see Secrets).
2. `bazel test --build_tests_only //backend/...` with the wrapped args.
3. `bazel query` for every rule under `//backend/...` _except_ OCI image/push rules (`oci_image|oci_push|pkg_tar_impl|directory_path|jq_rule|_copy_file|_write_file`) and then `bazel build` them. The query/build is the "everything that isn't a test target still has to compile" guard. OCI rules are excluded because they belong to the release flow, not PR validation.

### `run_cli_bazel_tests.sh`

1. `bazel test --build_tests_only //cli/...`.
2. `bazel build //cli/...` for untested binaries/libs.

There is no separate `bazel build //checks:flunge_checks` step — that target is not referenced here.

### `run_frontend_tests.sh`

`cd frontend && npm ci && npm run build && npm run test`. No bazel involvement. Uses whatever Node ships on the agent.

### `run_frontend_bazel_tests.sh`

`bazel test --build_tests_only //frontend/...` with the wrapped args, run as a separate step from `run_frontend_tests.sh`. Not mentioned in earlier versions of this doc's steps table.

### `run_ios_bazel_build.sh`

Local bazel only — no remote config. Sets `--disk_cache=<repo>/.buildkite-cache/bazel-disk` and `--repository_cache=$HOME/.cache/flunge/bazel-repo` (overrideable via `BUILDKITE_IOS_BAZEL_DISK_CACHE_PATH` / `BUILDKITE_IOS_BAZEL_REPOSITORY_CACHE_PATH`). Both `bazel build //mobile/ios/...` and `bazel test //mobile/ios/...` run against the whole package, not a single `FlungeApp` target. Paired with a buildkite cache plugin (emitted into the pipeline by the dispatcher when `BUILDKITE_IOS_CACHE_ENABLED=1`) so the disk-cache directory persists across runs.

### `decrypt_backend_config.sh`

Decrypts `backend/config-secrets.toml.gpg` in-tree. Reads `CONFIG_SECRET_KEY` from env first; falls back to `buildkite-agent secret get CONFIG_SECRET_KEY`. Hard-fails if neither path produces a key. Tolerant of the encrypted file being absent (some PRs don't need it).

### Release-flow scripts (not in PR CI)

- `release_orchestrator.sh staging|prod preflight|mutate` — staging/prod release pipelines call this. Runs the in-repo `release_orchestrator` bazel target with a yaml rules config.
- `release_frontend_swa.sh`, `release_frontend_promote.sh` — Cloudflare Pages + Azure Static Web Apps deploys.
- `release_ios_testflight.sh preflight|mutate` — uses `buildkite-agent meta-data set/get` to thread state between the two steps.

These run from separate pipeline files in `.buildkite/release-*.pipeline.yml`, each gated to a `linux-release` (or `macos-arm64`) queue with `concurrency: 1` + a `concurrency_group` so two releases never overlap.

**Not relevant to mono's PR CI.** Listed for completeness only.

## Bootstrap / pre-bootstrap

There is **no in-repo bootstrap hook**. The agents are pre-provisioned: bazel, Node, GitHub CLI, gpg, `buildkite-agent`, and the BuildBuddy creds are all expected to exist on the queue's agents. Toolchain pins (e.g., `.bazelversion` → bazelisk handles it; `.tool-versions` if present) are read by the tools themselves at job time. No `rustup`/`mise`/`asdf` step is run from CI scripts.

This matters for mono: mono will need rust on every agent (flunge agents don't have a rust toolchain pinned for the host). Whether to add an install-on-boot step or bake an agent image is the v1 question called out in the parent design's "Agent topology" section.

## Secrets surface

Secrets referenced (all sourced via buildkite's `secrets:` step key, which the agent exposes as env vars):

| Secret name                                                            | Used by                                                                                  | Purpose                                                                                                                                                                                                                                                                                   |
| ---------------------------------------------------------------------- | ---------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `REGISTRY_USERNAME` / `REGISTRY_PASSWORD`                              | `run_checks.sh`, both bazel-test scripts, all release scripts                            | Azure Container Registry (`flunge.azurecr.io`) pull creds. Declared on the step via buildkite's `secrets:` key; `lib.sh`'s `buildkite_bazel_args`/`buildkite_bazel_startup_args` do not read or inject them today (no `--config=remote`, no remote-exec headers are set from CI scripts). |
| `CONFIG_SECRET_KEY`                                                    | `decrypt_backend_config.sh`, prod/staging release                                        | Symmetric key for `backend/config-secrets.toml.gpg`.                                                                                                                                                                                                                                      |
| `GITHUB_TOKEN`                                                         | `run_checks.sh`, prod release, frontend release, frontend-promote, iOS-testflight mutate | PR comments, release-note authoring, asset uploads.                                                                                                                                                                                                                                       |
| `REGISTRY_LOGIN_SERVER`                                                | prod/staging mutate                                                                      | Container registry endpoint for image push.                                                                                                                                                                                                                                               |
| `AZURE_CREDENTIALS`, `AZURE_STORAGE_KEY`                               | prod mutate, frontend release                                                            | Azure deploy / storage.                                                                                                                                                                                                                                                                   |
| `CF_PAGES_ACCOUNT_ID`, `CF_PAGES_STAGING_TOKEN`, `CF_PAGES_PROD_TOKEN` | frontend release, frontend-promote                                                       | Cloudflare Pages deploy.                                                                                                                                                                                                                                                                  |
| `SWA_DEPLOYMENT_TOKEN`                                                 | frontend release                                                                         | Azure SWA deploy.                                                                                                                                                                                                                                                                         |
| `APP_STORE_CONNECT_KEY_ID` / `_ISSUER_ID` / `_PRIVATE_KEY`             | iOS-testflight mutate                                                                    | TestFlight upload auth.                                                                                                                                                                                                                                                                   |

Injection point is the buildkite step's `secrets:` array — agent-side, before the command runs, the agent exports them as env vars. No plugin involvement, no inline `${{ secrets.X }}` interpolation in YAML. `decrypt_backend_config.sh` shows the secondary path (`buildkite-agent secret get`) for when a step forgets to declare the secret; both work but declared-on-step is the convention.

The CI step never sees the GPG-decrypted config on disk persistently — `decrypt-config.sh` writes the plaintext into the working directory, the test step reads it, the agent's clean-between-jobs behaviour disposes of it.

## Bazel cache config

Three layers, each set in a different place:

1. **Repo-level disk cache** (`.bazelrc`): `build --disk_cache=~/.cache/bazelcache`. Applies to every bazel invocation, including local dev. Sticks per-agent.
2. **CI disk-cache configs** (`.ci.bazelrc`, imported from `.bazelrc`): `--config=ci-linux` / `--config=ci-darwin`, both selected by `buildkite_bazel_args` in `lib.sh` based on `uname -s`. Linux sets `--repository_cache=~/.cache/bazel-repo` (disk cache inherited from the top-level `.bazelrc`); Darwin points both `--disk_cache` and `--repository_cache` at a dedicated SSD volume (`/Volumes/ssd/bazel/...`). `.bazelrc` still defines a `config:remote` group (`--remote_cache`, `--remote_executor`, `--remote_header=x-buildbuddy-api-key=<token>` pointed at `*.buildbuddy.io`), but **no CI script activates it today** — `--config=remote` is not passed anywhere in `.buildkite/scripts/`, so regular PR CI runs bazel locally, not on BuildBuddy's remote executors.
3. **iOS local-only cache** (`run_ios_bazel_build.sh` + cache plugin): `--disk_cache=<repo>/.buildkite-cache/bazel-disk` plus a `--repository_cache`. The buildkite cache plugin restores the disk-cache directory across runs.

Cache invalidation is implicit (bazel hashes inputs); there's no explicit `bazel clean` in the pipeline scripts. The disk-cache size on iOS is configurable via `BUILDKITE_IOS_CACHE_SIZE` (default `40g`).

The org-scoped `x-buildbuddy-api-key` is checked into `.bazelrc`'s `config:remote` group in the public repo, unused by CI today. Treat that as flunge's intentional choice — the key is scoped + revocable — not a leak to replicate without thinking.

## Branch protection on flunge `main`

Live config (`gh api repos/brianduff/flunge/branches/main/protection` at audit time):

```json
{
  "required_status_checks": {
    "strict": false,
    "contexts": ["buildkite/flunge-ci"]
  },
  "enforce_admins": { "enabled": true },
  "allow_force_pushes": { "enabled": false },
  "allow_deletions": { "enabled": false },
  "required_linear_history": { "enabled": false },
  "required_signatures": { "enabled": false },
  "required_conversation_resolution": { "enabled": false }
}
```

Key facts:

- **One required context**: `buildkite/flunge-ci`. This is the umbrella name buildkite reports for the whole build. Per-step success/failure rolls up into this one check; branch protection never sees individual step names.
- `enforce_admins: true` — repo owner cannot merge through a red build either.
- `strict: false` — PRs are not required to be up-to-date with `main` before merge.
- No required reviewers, no required conversation resolution. The CI gate is the only merge gate.
- Force-push and branch deletion are disabled on `main`.

Ramp history: not recoverable from `git log` (branch-protection changes are not commits). The shape suggests flunge enabled `buildkite/flunge-ci` as required from day one rather than ramping; that matches what the org has done historically per agents-list lore but is not provable from the repo.

## Where mono should deliberately diverge

These are the points where copying flunge verbatim would be wrong for mono — flagged here so #2 doesn't have to re-derive them:

1. **Rust toolchain on the agent.** Flunge agents don't have rust. Mono needs it. Either install `rustup` from `rust-toolchain.toml` at job start (`bootstrap.sh` per the parent design), or bake it into a mono-tagged agent image. Parent design defers this to v2; v1 should ship the install-on-boot path.
2. **`jj` is a worker-side concern only.** Buildkite checks out via git; do not invoke `jj` from any CI script. The same constraint flunge has — flunge agents don't run `jj` either, even though humans use jj locally. There is no jj in flunge's `.buildkite/`.
3. **Required-check granularity.** Flunge gates on the umbrella `buildkite/flunge-ci`. The mono design currently proposes six per-step required checks (`buildkite/mono/bootstrap`, `buildkite/mono/cargo-check`, etc.). Pick one model deliberately:
   - Umbrella check (flunge-style): simpler branch-protection config, but a single failing step reddens the whole build and reviewers must drill into the buildkite UI to see which.
   - Per-step required checks (current mono proposal): more explicit signal in the GitHub UI, but renames are now a contract change (see parent design Q2). Branch protection has to be updated in lockstep with `pipeline.yml`.
     The reference implementation here (flunge) chose umbrella. The mono design has not. Flag for the reviewer.
4. **Dynamic vs. static pipeline.** Flunge emits steps conditionally based on path filtering — a docs-only PR runs `no-ci-steps` and goes green in seconds. The mono design as written assumes a fully static pipeline. For a small repo (mono today) this is fine; once mono grows enough that "do we need to rerun bazel for a frontend-only change?" matters, port flunge's `upload_dynamic_pipeline.sh` shape. Not a v1 requirement.
5. **BuildBuddy remote execution.** `.bazelrc` still defines a `config:remote` group pointed at BuildBuddy, but flunge's CI scripts don't activate it today — PR CI runs bazel with disk-cache only (`--config=ci-linux`/`--config=ci-darwin`), not on BuildBuddy's remote executors. The mono design proposes disk-cache-only v1, remote-cache-only v2 (no remote _execution_) — that's actually converging with, not diverging from, flunge's current state. Don't assume flunge is a remote-execution reference to copy; verify against the live scripts before citing it as prior art either way.
6. **iOS pipeline plumbing is not relevant.** Mono has no iOS surface today. Skip `macos-arm64` queue + the cache-plugin pattern for v1.
7. **Release pipelines stay out of scope.** Flunge has five separate release pipelines (prod/staging/frontend-swa/frontend-promote/ios-testflight). The mono "Boss CI" project covers PR gating only. Release pipelines for the macOS app or any future surface are a follow-up.

## File index (flunge, for the reader who wants to look at the source)

```
infrastructure/buildkite/pipelines/
  flunge-ci.stub.yml                             # 6 lines — the upload step (not under .buildkite/)
.buildkite/
  release-prod.pipeline.yml                      # prod release (preflight + mutate)
  release-prod-scheduled.pipeline.yml            # scheduled prod release trigger
  release-staging.pipeline.yml                   # staging release (preflight + mutate)
  release-frontend.pipeline.yml                  # SWA + Cloudflare Pages staging deploy
  release-frontend-promote.pipeline.yml          # promote frontend staging → prod
  release-ios-testflight.pipeline.yml            # TestFlight upload
  scripts/
    upload_dynamic_pipeline.sh                   # the dispatcher; reads changed paths, emits steps
    lib.sh                                       # shared helpers, base-ref detection, bazel-arg wrapper
    run_checks.sh                                # checkleft runner (via repobin, falling back to bin/checkleft)
    run_backend_bazel_tests.sh                   # bazel test //backend/... + non-OCI bazel build
    run_cli_bazel_tests.sh                       # bazel test //cli/... + bazel build //cli/...
    run_frontend_tests.sh                        # npm ci && build && test
    run_frontend_bazel_tests.sh                  # bazel test //frontend/...
    run_ios_bazel_build.sh                       # macOS, local cache only, build + test //mobile/ios/...
    decrypt_backend_config.sh                    # gpg-decrypt backend config in-tree
    release_orchestrator.sh                      # staging/prod release entry point
    release_frontend_swa.sh                      # Azure SWA + Cloudflare Pages deploy
    release_frontend_promote.sh                  # promote staging → prod tag-swap
    release_ios_testflight.sh                    # TestFlight uploader with meta-data threading
```

`.bazelrc` (top-level) — disk cache + a `config:remote` group are defined here; `.ci.bazelrc` (imported from it) adds the `ci-linux`/`ci-darwin` configs CI actually uses. The CI scripts opt in via `--config=ci-linux` or `--config=ci-darwin`, not `--config=ci --config=remote`.

There is a release-only pipeline noted above (`release-prod-scheduled.pipeline.yml`) not previously listed here; not relevant to mono's PR CI, same as the other release pipelines.

No `.buildkite/hooks/`, no agent-side scripts in this repo. All bootstrap is agent-image-provided.
