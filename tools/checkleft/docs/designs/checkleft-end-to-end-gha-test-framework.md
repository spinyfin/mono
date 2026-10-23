# Checkleft end-to-end GHA test framework

**Project:** P1609 / `proj_18b7e6e25f116b58_47a` — checkleft end-to-end GHA test framework
**Status:** Design (no implementation in this PR)
**Author:** Boss worker `exec_18b7e6f0d8c2fb48_47e`

## Goals

Checkleft has a growing surface that only behaves correctly inside a *real* CI environment: change-detection classification (PR vs merge-queue vs push), PR-description bypass tags, external WASM-check execution with wall-clock/epoch limits, and the `CHECKS_*` env plumbing that feeds the binary its context. Our current tests are unit/functional tests that drive a synthetic git repo on the local machine. They cannot exercise the part of checkleft that depends on an *actual GitHub Actions run* — the workflow YAML wiring, the `GITHUB_TOKEN` permissions, the `pull_request` event payload, the check-run/annotation reporting surface, and the interaction between a live PR description and the bypass machinery.

This project builds a small, ergonomic, end-to-end test framework that exercises checkleft in a **real GitHub Actions environment** — an actual test repository with a representative GHA workflow that invokes checkleft — rather than mocks or a local-only harness. Concretely:

1. **Real GHA execution.** Scenarios run against a real test repository whose GHA workflow invokes the checkleft-under-test, end to end, and we assert against what that real run produced.
2. **Programmatic scenario declaration.** Scenarios are declared in Rust (in mono, sharing checkleft's output types) with a small, ergonomic builder API — not YAML fixtures or shell.
3. **Triggerable for any mono PR or head.** The suite can run against the checkleft built from any mono PR touching `tools/checkleft/`, or from `main` HEAD. The caller either passes a prebuilt checkleft binary through to the test environment, or passes a mono commit sha from which the binary is built.
4. **Scenarios = baseline + mutations + assertions.** A scenario expresses repository mutations from a baseline, with assertions at each step against **both** a local checkleft run and the CI run. The baseline itself is expressible inside the scenario (e.g. "establish `somefile.rs` with this content, and a `CHECKS.yaml` that enables this check").
5. **Baseline restoration.** After each scenario, the test repo is restored to a stable baseline with no manual cleanup.
6. **Concurrency.** Scenarios run with a single, reliable concurrency model — parallel by isolation, with a serialization escape hatch.

### Why this is worth building now

The PR-description bypass path (`BYPASS_<CHECK_ID>=<reason>`) is a good example of a feature that *cannot* be regression-tested without real GHA. As of commit `81df757f` (#1465) we confirmed the bypass-via-PR-description path is **dead code on mono CI** — mono's Buildkite `checks.sh` never wires `CHECKS_PR_NUMBER`, so checkleft never fetches the PR description and the bypass never applies. That gap shipped silently precisely because there was no end-to-end test that opens a real PR, sets a real description, and asserts the downgrade. This framework is the missing coverage for that whole class of "only-real-in-CI" behavior.

## Non-goals

- **Not a replacement for unit/functional tests.** The existing in-process tests that drive a synthetic git repo (change-detection matrix, check logic, config resolution) stay. This framework covers only the behavior that requires a live GHA run; it is slow and quota-bound, so it tests breadth-of-integration, not every check's logic.
- **Not a general CI-provider matrix.** We exercise checkleft on **GitHub Actions** because that is what the operator requested ("a representative GHA setup") and what mono does not currently have. Buildkite coverage stays with the existing functional tests. We do not build a provider abstraction here.
- **Not building checkleft's release/distribution pipeline.** The checkleft release pipeline (`.buildkite/pipeline-checkleft-release.yml`, the `checkleft_musl` target) already exists and produces binaries. We *consume* a built binary; we do not redesign how checkleft is released. Reusing per-PR artifacts from that pipeline is called out as a future optimization, not a v1 requirement.
- **Not fork-PR / untrusted-contributor coverage.** All scenario PRs originate from branches **inside** the test repo (the `pull_request` event with a trusted `GITHUB_TOKEN`), not from forks. The `pull_request_target` security model and fork-PR scenarios are explicitly deferred.
- **Not a full CI-scenario matrix in v1.** v1 targets the `pull_request` event (which covers both example scenarios). `merge_group`, `push`-to-default, and shallow-clone CI scenarios are deferred and should later reuse the scenario→base matrix already designed in [`robust-change-detection-in-checkleft`](robust-change-detection-in-checkleft.md).
- **No mutation of the test repo's default branch by scenario runs.** Restoration and concurrency both depend on the default branch being read-only during runs; intentionally out of scope to ever write it from a scenario.

## Background: what a "real GHA run" gives us that local runs do not

A local `checkleft run --format=json` exercises the check logic and the change-detection classifier in `Local` mode, but it never produces:

- a **GHA workflow context** — `GITHUB_EVENT_NAME=pull_request`, the event payload JSON, `GITHUB_BASE_REF`, the shallow checkout, the `GITHUB_TOKEN` with scoped permissions;
- a **check-run conclusion + annotations** surfaced on a real PR;
- the **`CHECKS_*` env wiring** that a representative workflow must set for the bypass path to work (`CHECKS_PR_NUMBER`, `CHECKS_REPOSITORY`, and a token checkleft can use to read the PR body);
- the **artifact-upload** reporting surface a workflow uses to publish structured results.

checkleft already speaks the relevant interfaces today:

- **Structured output.** `checkleft run --format=json` prints `Vec<CheckResult>` where `CheckResult { check_id, findings: Vec<Finding> }` and `Finding { severity, message, location: Option<Location>, remediations, suggested_fix }`, with `Severity ∈ {error, warning, info}` (`tools/checkleft/src/output.rs`). Any `error` finding yields process exit 1; `warning`/`info` exit 0 (`tools/checkleft/src/main.rs`).
- **Bypass.** A `BYPASS_<CHECK_ID>=<reason>` tag in the commit or PR description downgrades matching findings to `warning` (`tools/checkleft/src/bypass.rs`). The PR description is resolved from `CHECKS_PR_DESCRIPTION`, else fetched by `CHECKS_PR_NUMBER` + `CHECKS_REPOSITORY` via the GitHub API, with the token chosen gh-first: `CHECKS_GITHUB_TOKEN` → `GH_TOKEN` → `GITHUB_TOKEN` (`tools/checkleft/src/main.rs::detect_github_token`).
- **Epoch limits.** External WASM checks get a proportional wall-clock budget `min(5000 + 100·n_files, 300_000)` ms; an over-budget check is trapped and reported as `"check \`X\` ... exceeded its N ms wall-clock limit"` — a hard error on stderr with a non-zero exit, not a `Finding` (`tools/checkleft/src/external/runtime.rs`).
- **Giant-structs check.** `rust-giant-structs-use-builder` flags structs above a field threshold (default 5; `rust_giant_struct_common.rs`). A struct with 10 fields is comfortably over the line and emits an `error` — the example-A trigger.

The framework's job is to wire a representative GHA workflow that sets all of the above the way a real consumer would, then assert against the structured results it produces.

## Alternatives considered

### Test-repo strategy

#### Alternative R-A — One persistent test repo, scenarios serialized against a shared default branch

A single dedicated test repo; each scenario mutates the default branch in place, asserts, then force-resets the branch back to a committed baseline.

**Rejected because:** restoration is fragile (force-pushing the default branch is destructive and racy), scenarios cannot run concurrently without corrupting each other's view of the branch, and an aborted run leaves the shared branch dirty for the next run. It fails the concurrency requirement and makes restoration a manual hazard.

#### Alternative R-B — Ephemeral repos created and destroyed per run via `gh api`

Create a fresh repo per suite run (or per scenario), push the scaffold + baseline, run, then delete the repo.

**Rejected as the default because:** every run must re-provision secrets/tokens/branch-protection and re-warm Actions, repo creation/deletion churns the org and bumps GitHub's repo-creation rate limits, and a crash leaks whole repos rather than just branches. It buys strong isolation we do not need for trusted, same-org scenarios. Kept as a **future** option for hostile/secret-sensitive scenarios.

#### Alternative R-C (chosen) — One persistent test repo, branch-per-scenario isolation

A single persistent dedicated test repo whose **default branch holds only the scaffold** (the representative GHA workflow + a minimal checkable project), never scenario source. Each scenario materializes its baseline on a fresh branch forked from the scaffold, applies mutations as further commits, opens its PR with `base = main, head = e2e/<run-id>/<scenario>`, and tears down by closing the PR and deleting the branch. **Chosen** because it gives restoration (the default branch is never written, so "restore to baseline" is automatic) and parallelism (branches/PRs are independent) almost for free, while keeping secrets and Actions provisioning one-time.

### Binary delivery

#### Alternative B-A (chosen) — Build once, deliver the same binary to both local and CI

The harness obtains one checkleft-under-test (host binary for local assertions + a `linux-musl` static binary for the GHA runner), uploads the linux binary as a content-addressed release asset on the test repo, and the workflow downloads it. Both the local and CI assertions in a scenario run the *same* checkleft build. **Chosen** because requirement 4 demands local-run *and* CI-run assertions in the same scenario be about the same binary, and a static musl binary needs no toolchain on the runner.

#### Alternative B-B — Pass a mono sha and have the GHA workflow build checkleft

The workflow checks out mono at a sha and `cargo`/`bazel build`s checkleft in the runner.

**Rejected because:** it drags a full Rust + bazel toolchain and a mono checkout into every test-repo CI run (minutes of build per scenario, large runners), couples the test repo to mono's build graph, and makes the local-vs-CI "same binary" guarantee harder (two independent builds). We still *support* a mono sha as an input, but resolve it to a binary on the harness side, not in the runner.

#### Alternative B-C — Consume prebuilt per-PR artifacts from mono CI

If mono CI publishes a reusable per-PR checkleft artifact (the release-pipeline work T1310/T1313/T1314 may grow this), the harness could download it instead of building.

**Deferred, not rejected:** the release pipeline today is cron/manual and produces *release* binaries, not per-PR artifacts. v1 builds from the sha/path itself (reusing the `checkleft_musl` target); when per-PR artifacts exist, swapping the binary-source is a localized change. Listed as **future**.

### Scenario authoring surface

#### Alternative S-A — YAML/JSON scenario fixtures interpreted by a runner

**Rejected because:** assertions need checkleft's own `Severity`/`Finding` types and matchers; a YAML DSL would re-encode them and drift. Requirement 2 explicitly asks for code with a small ergonomic API.

#### Alternative S-B (chosen) — Rust builder API in mono, sharing checkleft's output types

Scenarios are Rust tests using a builder (`Scenario::new(...).baseline(...).step(...)...`), parsing checkleft's `--format=json` into the real `output.rs` types and matching on them. **Chosen** for type-sharing, ergonomics, and running under the existing `cargo`/`bazel test` story.

## Chosen approach

### 1. Test repo strategy — persistent repo, branch-per-scenario (R-C)

A single dedicated repo, e.g. `spinyfin/checkleft-e2e` (final name is an open question — see the attentions manifest). Its **default branch (`main`) is the stable baseline of the *repository*** and holds only scaffold:

- the representative GHA workflow (`.github/workflows/checkleft.yml`),
- a minimal checkable project (a `Cargo.toml` and a placeholder `src/`) so checkleft always has something to scan,
- a `CHECKS.yaml` baseline,
- a README documenting that the repo is machine-managed.

**The scaffold is authored in mono**, under `tools/checkleft/e2e/testrepo/`, and pushed to the test repo's `main` by a `checkleft-e2e sync-scaffold` command. This keeps the workflow YAML reviewable in mono PRs and makes the test repo a deterministic projection of mono, not a hand-edited island.

Per-scenario flow:

1. Fork a fresh branch `e2e/<run-id>/<scenario-slug>` from `main` (so it inherits the workflow + project plumbing — required, because a `pull_request` run uses the **head branch's** workflow file).
2. **Baseline step** commits the scenario's baseline files (e.g. `src/somefile.rs`, a `CHECKS.yaml` enabling the relevant check). The baseline is part of the scenario, satisfying requirement 4.
3. Mutation steps commit further changes.
4. `open_pr()` opens a PR `base = main, head = e2e/<run-id>/<scenario>`.
5. Teardown closes the PR and deletes the branch.

**Restoration (requirement 5) is automatic:** because no scenario ever writes `main`, the repo is always at its stable baseline; teardown only removes the scenario's own branch/PR. A per-scenario RAII guard performs teardown even on panic/failure, and a janitor pass (below) sweeps anything an aborted process left behind.

### 2. Binary delivery (B-A) — build once, content-addressed release-asset drop

The harness accepts the checkleft-under-test two ways (requirement 3):

- **binary mode:** caller passes paths to a prebuilt host binary and a `linux-musl` binary.
- **sha mode:** caller passes a mono commit sha; the harness checks out / is already at that sha and builds both `//tools/checkleft:checkleft` (host) and `//tools/checkleft:checkleft_musl` (static linux) with bazel.

It then:

1. Computes the SHA-256 of the linux binary and uploads it as a **content-addressed release asset** on the test repo under a tag like `bin/<sha256-prefix>` (write-once; re-uploading an identical binary is idempotent and collision-free, which makes concurrent scenarios that share a binary safe).
2. Triggers each scenario's PR/workflow, passing the asset tag as a `workflow_dispatch` input / committed pointer file so the workflow's first step is `gh release download bin/<hash> && chmod +x checkleft`.
3. Runs **local** assertions with the host binary directly.

Local and CI thus exercise the identical build from the identical source.

### 3. Scenario API (S-B)

A new crate `tools/checkleft/e2e/` (depends on the checkleft lib crate for `output` types). Sketch:

```rust
#[tokio::test]
async fn giant_struct_then_suppress() {
    Scenario::new("giant-struct-then-suppress")
        // Baseline is part of the scenario: files + config on a fresh branch.
        .baseline(|repo| {
            repo.write("Cargo.toml", BASELINE_CARGO);
            repo.write("CHECKS.yaml", indoc! {r#"
                checks:
                  - id: rust-giant-structs-use-builder
                    config: { max_fields: 5 }
            "#});
            repo.write("src/somefile.rs", "pub struct Small { a: u8, b: u8 }\n");
        })
        // Step 1 — local assertion only.
        .step("introduce a 10-field struct", |s| {
            s.write("src/somefile.rs", STRUCT_WITH_10_FIELDS);
            s.assert_local(Findings::expect().one(
                Match::check("rust-giant-structs-use-builder")
                    .severity(Severity::Error)
                    .path("src/somefile.rs"),
            ));
        })
        // Step 2 — open PR, assert the real CI run.
        .step("CI flags the violation", |s| {
            s.open_pr();
            s.assert_ci(CiResult::expect()
                .conclusion(Conclusion::Failure)
                .findings(Findings::expect().one(
                    Match::check("rust-giant-structs-use-builder").severity(Severity::Error))));
        })
        // Step 3 — edit PR description; CI re-runs (workflow listens for `edited`).
        .step("suppression downgrades to warning", |s| {
            s.set_pr_description("BYPASS_RUST_GIANT_STRUCTS_USE_BUILDER=accepted for test");
            s.assert_ci(CiResult::expect()
                .conclusion(Conclusion::Success)
                .findings(Findings::expect().one(
                    Match::check("rust-giant-structs-use-builder").severity(Severity::Warning))));
        })
        .run().await; // teardown via RAII even on assertion failure
}
```

```rust
#[tokio::test]
async fn custom_check_hits_epoch_limit() {
    Scenario::new("epoch-limit")
        .baseline(|repo| {
            repo.write("Cargo.toml", BASELINE_CARGO);
            // A prebuilt custom WASM check that burns ~60s of CPU, registered via check_definitions.
            repo.write_bytes("checks/slow-check/check.wasm", fixtures::SLOW_CHECK_WASM);
            repo.write("checks/slow-check/check.yaml", SLOW_CHECK_MANIFEST);
            repo.write("CHECKS.yaml", indoc! {r#"
                check_definitions: { exec_paths: [checks] }
                checks: [ { id: slow-check } ]
            "#});
            repo.write("src/lib.rs", "// give the check a file to scan\n");
        })
        .step("local run trips the epoch limit", |s| {
            s.write("src/lib.rs", "pub fn touched() {}\n");
            s.assert_local(RunOutcome::expect()
                .failed()
                .stderr_contains("exceeded its")
                .stderr_contains("wall-clock limit"));
        })
        .step("CI run trips the epoch limit", |s| {
            s.open_pr();
            s.assert_ci(CiResult::expect()
                .conclusion(Conclusion::Failure)
                .run_error_contains("wall-clock limit"));
        })
        .run().await;
}
```

API primitives:

| Primitive | Effect |
| --- | --- |
| `.baseline(\|repo\| …)` | first commit on the scenario branch: `repo.write(path, content)`, `repo.write_bytes(path, &[u8])`, `repo.delete(path)` |
| `.step(name, \|s\| …)` | a named mutation+assertion unit; mutations (`s.write`/`s.delete`) commit to the branch |
| `s.assert_local(matcher)` | run the host binary over the current tree, parse `--format=json`, match findings / `RunOutcome` (exit code + stderr) |
| `s.open_pr()` | push branch, open PR, remember the PR handle |
| `s.assert_ci(CiResult)` | wait for the PR's checkleft check-run to complete, download the structured result bundle, match conclusion + findings/error |
| `s.set_pr_description(text)` | PATCH the PR body via the GitHub API (re-triggers CI via the workflow's `edited` type) |
| matchers | `Match::check(id).severity(sev).path(p).line(n).message_contains(s)`; `Findings::expect().one(..)/.none()/.all_of(..)`; `RunOutcome::expect().failed().stderr_contains(..)`; `CiResult::expect().conclusion(..).findings(..).run_error_contains(..)` |

The two example scenarios above are the **verbatim** mapping of operator example A and example B.

### 4. Assertion surface — structured over log-grepping

- **Local.** Run `checkleft run --format=json`; capture stdout (parsed into `Vec<CheckResult>`), stderr, and exit code. Match on the parsed findings for the happy path and on `(exit_code, stderr)` for hard-error paths (epoch limit).
- **CI.** The representative workflow wraps the run and uploads a **result bundle** artifact, so assertions never grep logs:

  ```yaml
  - name: run checkleft
    id: run
    run: |
      set +e
      ./checkleft run --format=json > findings.json 2> stderr.txt
      echo "exit=$?" > meta.txt
  - uses: actions/upload-artifact@v4
    with: { name: checkleft-result, path: "findings.json\nstderr.txt\nmeta.txt" }
  ```

  The harness asserts against three things: the **check-run conclusion** (coarse pass/fail/neutral), the parsed **`findings.json`** (severity/check-id/location — covers the giant-struct and bypass-downgrade cases), and **`meta.txt` exit code + `stderr.txt`** (covers the epoch-limit hard error, which is not a `Finding`). This makes both example scenarios assertable structurally.

The representative workflow must set, the way a real consumer would: `permissions: { contents: read, pull-requests: read, checks: write }`; `CHECKS_PR_NUMBER=${{ github.event.pull_request.number }}`, `CHECKS_REPOSITORY=${{ github.repository }}`, `GITHUB_TOKEN` exported so checkleft's gh-first detection finds it; `pull_request: { types: [opened, synchronize, reopened, edited] }` so a description edit re-triggers the run (the suppression step depends on this). Wiring these is itself under test — it is exactly the wiring missing on mono CI (commit `81df757f`).

### 5. Trigger model

- **Local / manual (documented).** A thin driver: `cargo test -p checkleft-e2e` (or `bazel test //tools/checkleft/e2e/...`), or a `checkleft-e2e run --scenario <name> [--binary <path> | --mono-sha <sha>]` binary. Runs from any machine with `gh` auth for the test repo. This is the primary developer entry point.
- **mono CI, label/comment-gated (cost control).** A Buildkite step (mono is Buildkite, not GHA) that runs **only** when a PR touching `tools/checkleft/` carries the `e2e-checkleft` label or a `/checkleft-e2e` comment. It builds checkleft from the PR HEAD (binary mode), runs the suite, and posts a summary via `gh pr comment`. Gated because each run spins real GHA runs and polls for minutes — not something to pay on every PR.
- **Scheduled at head.** A cron Buildkite pipeline (modeled on `pipeline-checkleft-release.yml`) runs the full suite against `main`'s checkleft nightly, catching drift independent of any PR.

The mono-side runner never ships binary *bytes* through a `workflow_dispatch` input (inputs are strings); it ships the **content-addressed asset tag** and lets the workflow download. Results surface as the Buildkite step's pass/fail plus a PR comment for the gated case.

### 6. Concurrency model — parallel by isolation, serialize on demand

**Default: parallel via branch/PR isolation**, resting on one invariant: *nothing mutates the test repo's default branch during runs*. Each scenario owns a unique branch namespace `e2e/<run-id>/<scenario-slug>`, its own PR, and its own result artifacts (keyed by run id). The only shared state is `main` (read-only) and the binary asset drop (content-addressed, write-once → concurrent identical uploads are safe). No lock is needed for correctness.

A **serialization flag** is provided for debugging (run scenarios one at a time). Quota/limit handling:

- **Concurrency cap.** Bound simultaneously-running scenarios (config, default e.g. 4) to stay within the test repo's Actions concurrency and the 5000 req/hr REST limit.
- **CI polling.** Poll check-runs on a backoff (e.g. 5s → 15s) with a per-scenario hard ceiling (default 10 min). On timeout the scenario fails with a precise message *and still tears down*.
- **Slow scenarios budgeted.** Example B's check burns ~60s of CPU, but the epoch trap fires at `≈5000 + 100·n_files` ms (~5s for a tiny changeset), so the run returns in seconds, not a minute — the poll ceiling accommodates worst case regardless.

### 7. Auth

- **In the test repo's workflow:** the default `GITHUB_TOKEN` (same-repo `pull_request`) — it can read the PR it runs for and upload artifacts. checkleft's gh-first detection consumes it for the PR-description read. No extra secret needed inside the run.
- **Driving the repo from outside (open/edit/close PRs, dispatch, read check-runs, download artifacts, upload assets):**
  - **CI:** a **GitHub App** installed on the test repo (short-lived, repo-scoped installation tokens, no human PAT to rotate). The App private key lives in mono's Buildkite secret store; nothing committed. (PAT is the lighter-weight alternative — see the attentions manifest.)
  - **Local:** the developer's existing `gh auth` token (the **gh-first direction**, consistent with how checkleft already prefers `GH_TOKEN`). A developer authenticated with `gh` runs the suite with zero extra setup.

### 8. Cleanup guarantees

- **Per-scenario RAII teardown** (close PR, delete branch, delete scenario-scoped artifacts) runs on success, failure, and panic.
- **Janitor / TTL sweep.** A scheduled cron (a GHA scheduled workflow in the test repo, or a mono Buildkite cron) deletes any `e2e/*` branch and closes any `e2e/*` PR older than a TTL (default 6h), and prunes `bin/*` release assets past TTL. The `e2e/<run-id>/…` namespace makes janitor matching unambiguous and impossible to confuse with human branches. This catches aborted runs where the RAII guard never fired (killed CI, crashed machine).

## Risks / open questions

1. **Epoch interruption of a "sleeping" check.** wasmtime epoch interruption traps on wasm loop back-edges / function entries; a guest blocked inside a *host* call (a real WASI sleep/poll) may not be interrupted mid-call. To make example B deterministic the fixture should **spin on CPU** for ~60s rather than call a blocking sleep. *Open: confirm checkleft's epoch wiring interrupts a CPU-bound guest reliably across the runner's wasmtime version, and decide whether a true host-blocking sleep is also in scope (deferred F-item if not).*
2. **`pull_request` description-edit re-trigger.** The suppression step depends on the workflow listening for `edited`. If we would rather not run CI on every description edit in real consumer repos, the alternative is the harness pushing an empty commit (a `synchronize` event) after editing the body. *Open: `edited` in the test repo's workflow (chosen) vs. empty-commit re-trigger.*
3. **Test-repo provisioning is a one-time human/ops step.** Creating the repo, installing the App (or minting the PAT), and seeding branch protection (none, so scenario branches are freely deletable) are not code. *Open: who owns provisioning and where the App credential is stored (see attentions).*
4. **GHA minutes / quota.** Real runs consume Actions minutes; a chatty suite at high concurrency could exhaust them. Mitigated by the concurrency cap and label-gating. *Open: GitHub-hosted runners vs. a self-hosted pool if minutes become the bottleneck (future F-item).*
5. **Binary platform coverage.** v1 runs CI on linux GHA runners (static musl). Local assertions run on the host (macOS arm64 for most developers, linux on CI agents). If a scenario's behavior is platform-sensitive, local-vs-CI may diverge. *Open: is linux-CI + host-local acceptable for v1, or do we need a linux local run too?*
6. **Result-bundle vs. annotations.** We prefer the uploaded `findings.json` bundle as the assertion source. Check-run **annotations** are a secondary, lossy signal we assert only coarsely. *Open: do we also want the workflow to emit annotations and assert a sample, to cover the annotation-rendering path?*
7. **Sharing the CI-scenario matrix.** v1 covers `pull_request` only. The `merge_group`/`push`/shallow scenarios should reuse the matrix from [`robust-change-detection-in-checkleft`](robust-change-detection-in-checkleft.md) rather than re-deriving it. *Open: fold those in as a fast-follow or hold for a separate project?*

## Proposed implementation task breakdown

Tasks are PR-sized and listed in dependency order. "Depth" marks tasks with no edge between them that may run in parallel. Each task names its scope, an effort hint, and explicit dependencies by task name.

### Depth 0 (no dependencies — may run in parallel)

**T1. Test-repo scaffold + representative GHA workflow (authored in mono)**
Scope: Add `tools/checkleft/e2e/testrepo/` containing the scaffold pushed to the test repo's `main`: `.github/workflows/checkleft.yml` (the representative workflow — `pull_request: [opened, synchronize, reopened, edited]` + `workflow_dispatch`; `permissions: contents/pull-requests read, checks write`; `CHECKS_PR_NUMBER`/`CHECKS_REPOSITORY`/`GITHUB_TOKEN` wiring; the `gh release download bin/<hash>` step; the result-bundle `upload-artifact` step), a minimal checkable project, and a `CHECKS.yaml` baseline. Document the one-time repo + auth provisioning. Note this materially exercises the `CHECKS_*` wiring missing on mono CI (commit `81df757f`).
Effort: medium. Dependencies: none.

**T2. Expose checkleft output types as a reusable library surface**
Scope: Ensure `output.rs`'s `CheckResult`/`Finding`/`Severity`/`Location` are re-exported from the checkleft library crate (and `Deserialize`-able) so the e2e crate can parse `--format=json` and match against the real types rather than a parallel copy. Pure library plumbing; no behavior change.
Effort: small. Dependencies: none.

### Depth 1

**T3. Binary delivery plumbing**
Scope: Harness functions that, given a prebuilt binary path *or* a mono sha, produce `(host, linux-musl)` binaries (build via `//tools/checkleft:checkleft` and `:checkleft_musl`), SHA-256 the linux binary, upload it as a content-addressed `bin/<hash>` release asset, and provide the workflow-side download contract. Idempotent re-upload of identical bytes.
Effort: medium. Dependencies: T1.

**T4. e2e harness crate skeleton + gh-driven test-repo client**
Scope: New `tools/checkleft/e2e/` crate. A GitHub client (App-token in CI, gh-token locally) over branch create/delete, PR open/edit/close, `workflow_dispatch`, check-run polling with backoff + ceiling, and artifact download. Per-scenario RAII teardown guard. `run-id`/branch-namespace allocation.
Effort: large. Dependencies: T1, T2. (May run in parallel with T3.)

### Depth 2

**T5. Scenario builder API**
Scope: Implement the `Scenario`/`step`/matcher surface from §3 on top of T4: `baseline`, mutation steps, `assert_local` (host binary + JSON/`RunOutcome` matching), `open_pr`, `assert_ci` (conclusion + result-bundle parsing), `set_pr_description`, and the `Match`/`Findings`/`CiResult`/`RunOutcome` matchers.
Effort: large. Dependencies: T4, T2.

**T6. Custom-check fixtures for the epoch scenario**
Scope: A prebuilt CPU-spinning WASM check component (built once via the checkleft SDK; a bazel target emits `slow_check.wasm`) plus its `check.yaml` manifest, embedded as a fixture the harness can drop into a scenario baseline. Spin (not host-sleep) so epoch interruption is deterministic (risk 1).
Effort: medium. Dependencies: T4. (May run in parallel with T5.)

### Depth 3

**T7. Scenario A — giant-struct → local violation → CI violation → bypass downgrade**
Scope: Implement operator example A verbatim against `rust-giant-structs-use-builder`: baseline establishes `somefile.rs` + enabling `CHECKS.yaml`; mutate to a 10-field struct; assert local `error`; open PR, assert CI `error`/conclusion failure; edit PR description with `BYPASS_RUST_GIANT_STRUCTS_USE_BUILDER=…`; assert CI `warning`/conclusion success.
Effort: medium. Dependencies: T5, T3.

**T8. Scenario B — custom check hits the epoch limit (local + CI)**
Scope: Implement operator example B verbatim: baseline registers the T6 spinning check; assert local run fails with the `"exceeded its … wall-clock limit"` stderr; open PR, assert CI conclusion failure with the same run-error captured in the result bundle.
Effort: medium. Dependencies: T5, T6, T3. (May run in parallel with T7.)

### Depth 4

**T9. Trigger integration (mono Buildkite gated step + scheduled head pipeline)**
Scope: A label/comment-gated mono Buildkite step that builds checkleft from PR HEAD and runs the suite, posting a `gh pr comment` summary; plus a cron Buildkite pipeline running the suite against `main` nightly. Documents the manual `checkleft-e2e run` entry point.
Effort: medium. Dependencies: T7, T8, T3.

**T10. Janitor / TTL cleanup sweep**
Scope: A scheduled sweep (GHA scheduled workflow in the test repo or a mono Buildkite cron) that closes stale `e2e/*` PRs, deletes stale `e2e/*` branches, and prunes old `bin/*` assets past a TTL — the safety net for aborted runs the RAII guard missed.
Effort: small. Dependencies: T1, T4. (Independent of T5–T9; may start as early as depth 2 and is placed late only because it is a non-blocking safety net.)

### Deferred / future (not a v1 blocker)

- **F1. Consume mono-CI per-PR prebuilt artifacts** instead of building in the harness, once the release-pipeline work (T1310/T1313/T1314) produces reusable per-PR artifacts. `future / not a v1 blocker`.
- **F2. Self-hosted runner pool** for the test repo if GHA minutes/quota become the bottleneck (risk 4). `future / not a v1 blocker`.
- **F3. Ephemeral-repo isolation mode** (R-B) for hostile/secret-sensitive scenarios needing repo-level isolation. `future / not a v1 blocker`.
- **F4. Additional CI-scenario coverage** (`merge_group`, `push`-to-default, shallow clone) reusing the change-detection matrix from [`robust-change-detection-in-checkleft`](robust-change-detection-in-checkleft.md). `future / not a v1 blocker`.
- **F5. Fork-PR / `pull_request_target` security-model scenarios** (untrusted contributors). `future / not a v1 blocker`.
- **F6. True host-blocking-sleep epoch interruption** (vs. CPU-spin) if/when checkleft guarantees interruption of blocked host calls (risk 1). `future / not a v1 blocker`.
- **F7. Annotation-rendering assertions** — assert a sample of check-run annotations in addition to the structured bundle (risk 6). `future / not a v1 blocker`.
